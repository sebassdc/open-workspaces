#!/bin/bash
# Smoke test: deploy one host, run the workspace lifecycle (create, connect,
# snapshot, fork, live fork, hibernate/resume, stop, delete) over SSM, record
# timings, then always `pulumi destroy`. Every input comes from the environment:
#
#   AWS credentials and AWS_REGION   standard AWS SDK variables (required)
#   Pulumi backend                   your own `pulumi login` or PULUMI_BACKEND_URL;
#                                    PULUMI_CONFIG_PASSPHRASE for self-managed backends
#   OW_STACK                         new stack name (default: smoke); an existing stack is refused
#   OW_REF                           git tag, commit or branch built on the host (default: main)
#   OW_REPOSITORY                    git URL (default: the upstream repository)
#   OW_INSTANCE_TYPE                 default m8i-flex.xlarge
#   OW_SUBNET_ID                     default: default VPC subnet offering the type
#   OW_DATA_GIB                      Btrfs data volume size (default 100)
#   OW_TAILSCALE_SECRET_ARN          optional Tailscale auth-key secret; OW_TAILSCALE_SSH=1 for Tailscale SSH
#   OW_SAMPLES                       lifecycle repetitions, 1-30 (default 5)
#   OW_RESULTS_DIR                   where results go (default: ./results, git-ignored)
#   OW_KEEP=1                        skip destroy, for debugging only (rerun needs a new OW_STACK)
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$here"
: "${AWS_REGION:?set AWS_REGION}"
stack=${OW_STACK:-smoke}
results=${OW_RESULTS_DIR:-$here/results}/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$results"
[[ -d ../../node_modules ]] || (cd ../.. && npm ci --no-audit --no-fund)

now() { date +%s; }
cleanup() {
  status=$?
  if [[ "${OW_KEEP:-}" == 1 ]]; then
    echo "OW_KEEP=1: stack $stack left running; destroy it with: pulumi destroy -s $stack" >&2
  else
    # Leave the tailnet cleanly instead of leaving an offline device behind.
    [[ -n "${OW_TAILSCALE_SECRET_ARN:-}" && -n "${instance:-}" ]] && { ssm 60 'tailscale logout' > /dev/null 2>&1 || true; }
    pulumi destroy -s "$stack" --yes --skip-preview > "$results/destroy.log" 2>&1 \
      && pulumi stack rm -s "$stack" --yes > /dev/null \
      || { echo "DESTROY FAILED: see $results/destroy.log" >&2; status=1; }
  fi
  exit "$status"
}
# Only a stack this run creates may be destroyed: `stack init` refuses an existing name.
pulumi stack init "$stack" > /dev/null \
  || { echo "stack $stack exists or cannot be created; pick a new OW_STACK. Nothing was changed or destroyed." >&2; exit 1; }
trap cleanup EXIT

started=$(now)
pulumi up -s "$stack" --yes --skip-preview > "$results/up.log" 2>&1 || { tail -20 "$results/up.log"; false; }
instance=$(pulumi stack output -s "$stack" instanceId)
provisioned=$(now)
echo "instance $instance up in $((provisioned - started)) s"

ssm() { # ssm TIMEOUT_SECONDS SCRIPT: run as root on the host, print stdout
  local id state
  id=$(aws ssm send-command --instance-ids "$instance" --document-name AWS-RunShellScript \
    --timeout-seconds 600 --parameters "$(python3 -c 'import json,sys; print(json.dumps({"commands":[sys.argv[1]],"executionTimeout":[sys.argv[2]]}))' "$2" "$1")" \
    --query Command.CommandId --output text)
  while :; do
    sleep 5
    state=$(aws ssm get-command-invocation --command-id "$id" --instance-id "$instance" \
      --query Status --output text 2>/dev/null || echo Pending)
    case $state in Pending|InProgress|Delayed) ;; *) break ;; esac
  done
  aws ssm get-command-invocation --command-id "$id" --instance-id "$instance" \
    --query '[StandardOutputContent,StandardErrorContent]' --output text
  [[ $state == Success ]]
}

until [[ "$(aws ssm describe-instance-information --filters "Key=InstanceIds,Values=$instance" \
  --query 'InstanceInformationList[0].PingStatus' --output text)" == Online ]]; do
  (( $(now) - provisioned < 900 )) || { echo "SSM agent never came online"; false; }
  sleep 10
done
ssm_online=$(now)
echo "SSM online after $((ssm_online - provisioned)) s"

while :; do
  state=$(ssm 60 'cat /var/lib/open-workspaces/bootstrap-status 2>/dev/null || echo pending' | head -1)
  case $state in ok*) break ;; failed) ssm 60 'tail -40 /var/log/open-workspaces-bootstrap.log'; false ;; esac
  (( $(now) - provisioned < 3600 )) || { echo "bootstrap still $state after an hour"; false; }
  sleep 30
done
bootstrapped=$(now)
echo "bootstrap $state after $((bootstrapped - provisioned)) s"

script=$(base64 < lifecycle.sh | tr -d '\n')
ssm 3000 "echo $script | base64 -d > /tmp/ow-lifecycle.sh && OW_SAMPLES=${OW_SAMPLES:-5} bash /tmp/ow-lifecycle.sh" > "$results/lifecycle.out" \
  || { cat "$results/lifecycle.out"; false; }
python3 - "$results" "$((provisioned - started))" "$((ssm_online - provisioned))" "$((bootstrapped - provisioned))" \
  "$(aws ec2 describe-instances --instance-ids "$instance" --query 'Reservations[0].Instances[0].[InstanceType,Placement.AvailabilityZone,ImageId]' --output text)" <<'SUMMARY'
import json, sys
results, provision, ssm, bootstrap, placement = sys.argv[1:]
report = next(json.loads(line) for line in open(f"{results}/lifecycle.out") if line.startswith("{"))
instance_type, zone, image = placement.split()
report["host"] = {"instance_type": instance_type, "availability_zone": zone, "image": image,
                  "provision_s": int(provision), "ssm_online_s": int(ssm), "bootstrap_s": int(bootstrap)}
open(f"{results}/summary.json", "w").write(json.dumps(report, indent=2) + "\n")
print(json.dumps(report, indent=2))
SUMMARY
echo "results: $results"

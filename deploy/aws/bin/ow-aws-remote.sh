# shellcheck shell=bash
# Functions ow-aws runs as root on the host, over SSM: it sends this file, then
# one call. Works on every host the Host component made; hosts that ship
# ow-host (layout v2) take ow-host's own commands where ow-aws prefers them.
set -euo pipefail

resume_list=/var/lib/open-workspaces/resume-on-start
bootstrap_status=/var/lib/open-workspaces/bootstrap-status
guest=/srv/ow/src/data/runtime-spike/guest

running_ids() {
  ow list | python3 -c 'import json,sys; print(" ".join(w["id"] for w in json.load(sys.stdin) if w["state"] == "running"))'
}

# Hibernates every running box and remembers it for resume_boxes. A box that
# cannot be hibernated, or a box list that cannot be read, stops the script.
hibernate_boxes() {
  local ids box
  # Assigned first: a failure inside `for box in $(...)` would read as no boxes.
  ids=$(running_ids)
  for box in $ids; do
    ow hibernate "$box" > /dev/null
    echo "$box" >> "$resume_list"
    echo "hibernated $box"
  done
}

# Waits up to $1 seconds for cloud-init, then checks what the bootstrap leaves:
# its status file, the ow user and the controller unit. A replaced host keeps
# its data volume (images built) but has none of these until the bootstrap ends.
bootstrap_done() {
  local rc=0
  # Exit 2 is cloud-init's done-with-warnings; the checks below still catch an unfinished bootstrap.
  timeout "$1" cloud-init status --wait > /dev/null || rc=$?
  if (( rc == 124 )); then echo "cloud-init still running after $1 s"; return 1; fi
  if (( rc != 0 && rc != 2 )); then echo "the host bootstrap (cloud-init) failed:"; cloud-init status --long; return 1; fi
  local status
  status=$(cat "$bootstrap_status" 2>/dev/null || echo missing)
  if [[ $status != ok* ]]; then
    echo "bootstrap status: $status (log: /var/log/open-workspaces-bootstrap.log)"
    tail -n 20 /var/log/open-workspaces-bootstrap.log 2>/dev/null || true
    return 1
  fi
  id ow > /dev/null 2>&1 || { echo "no ow user"; return 1; }
  systemctl cat open-workspaces > /dev/null 2>&1 || { echo "no open-workspaces unit"; return 1; }
}

# Waits up to $1 seconds for the developer image build (component input
# developerImages), which runs in the background after a fresh data volume.
images_done() {
  systemctl cat open-workspaces-images > /dev/null 2>&1 \
    || { echo "no open-workspaces-images unit: the host has no developerImages"; return 1; }
  local deadline=$((SECONDS + $1))
  while [[ $(systemctl show -p ActiveState --value open-workspaces-images) == activating ]]; do
    (( SECONDS < deadline )) || { echo "the developer image is still building after $1 s"; return 1; }
    sleep 5
  done
  python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("developer_user") else 1)' "$guest/ubuntu.json" 2>/dev/null \
    || { echo "the ubuntu developer image is not built:"; systemctl status open-workspaces-images --no-pager | head -n 5; return 1; }
}

runner_online() {
  ow node list | python3 -c 'import json,sys; sys.exit(0 if any(n["online"] for n in json.load(sys.stdin)) else 1)'
}

# The tailnet address once Tailscale is Running, "wait" while it connects, "none" when not joined.
tailnet() {
  tailscale status --json 2>/dev/null | python3 -c '
import json, sys
s = json.load(sys.stdin)
state = s.get("BackendState")
ips = [i for i in s.get("TailscaleIPs") or [] if "." in i]
print("wait" if state in ("NoState", "Starting") else ips[0] if state == "Running" and ips else "none")' 2>/dev/null || echo none
}

# Waits up to $1 seconds until the worker answers, the runner is online (right
# after a restart the old heartbeat still reads online, hence the worker check)
# and the controller listens on the tailnet address when the host is on the
# tailnet. `ow host up` picks that address once, at start: if it started before
# the tailnet, it is restarted while no box runs.
wait_ready() {
  local deadline=$((SECONDS + $1)) t addr='' rebound=''
  while :; do
    t=$(tailnet)
    if [[ $t != wait ]] && ow list > /dev/null 2>&1 && runner_online 2>/dev/null; then
      addr=$(ss -Hltn sport = :8790 | awk '{print $4}' | head -n 1)
      if [[ -n $addr && ( $t == none || $addr == "$t:8790" ) ]]; then break; fi
      if [[ -n $addr ]]; then
        local ids
        ids=$(running_ids)
        if [[ -n $ids ]]; then
          echo "the controller listens on $addr, not the tailnet address $t, and boxes are running: run ow-aws restart"
          return 1
        fi
        if [[ -z $rebound ]]; then
          echo "the controller started before the tailnet ($addr); restarting it on $t"
          systemctl restart open-workspaces
          rebound=1
        fi
      fi
    fi
    (( SECONDS < deadline )) || { echo "runner not ready after $1 s"; journalctl -u open-workspaces -n 20 --no-pager; return 1; }
    sleep 2
  done
  echo "controller listening on: $addr"
  local url
  url=$(tailscale serve status 2>/dev/null | grep -o 'https://[^ ]*' | head -n 1 || true)
  [[ -z $url ]] || echo "dashboard: $url"
}

# Starts the boxes hibernate_boxes remembered. A box that fails to resume stays
# in the list, so the next start or restart retries it.
resume_boxes() {
  [[ -f $resume_list ]] || return 0
  local states box failed=()
  states=$(ow list | python3 -c 'import json,sys; print(" ".join(w["id"] + "=" + w["state"] for w in json.load(sys.stdin)))')
  # shellcheck disable=SC2013 # box IDs are single words
  for box in $(sort -u "$resume_list"); do
    case " $states " in
      *" $box=running "*) echo "$box is already running" ;;
      *" $box="*) if ow start "$box" > /dev/null; then echo "resumed $box"; else echo "could not resume $box"; failed+=("$box"); fi ;;
      *) echo "$box no longer exists; dropped" ;;
    esac
  done
  if (( ${#failed[@]} )); then
    printf '%s\n' "${failed[@]}" > "$resume_list"
    echo "still to resume (kept in $resume_list): ${failed[*]}"
    return 1
  fi
  rm -f "$resume_list"
}

# ready WAIT IMAGES: the readiness wait; IMAGES non-empty also waits for the developer image.
ready() {
  if command -v ow-host > /dev/null; then
    ow-host ready --wait "$1" ${2:+--images}
    return
  fi
  bootstrap_done "$1"
  [[ -z $2 ]] || images_done "$1"
  wait_ready "$1"
}

# restart WORKER: the controller only where ow-host can restart it alone; else
# the whole service, with running boxes hibernated first and resumed after.
restart() {
  if command -v ow-host > /dev/null; then
    ow-host restart "$([[ -n $1 ]] && echo --worker || echo --control-plane)"
    return
  fi
  hibernate_boxes
  systemctl restart open-workspaces
  wait_ready 180
  resume_boxes
}

# After an instance start: ready, then the boxes hibernate_boxes remembered.
after_start() {
  ready "$1" ""
  resume_boxes
}

# After `pulumi up`: as after_start, then config and release (hosts with ow-host).
post_up() {
  after_start "$1"
  shift
  if command -v ow-host > /dev/null; then
    ow-host apply
    ow-host upgrade "$@"
  fi
}

ow_host() {
  command -v ow-host > /dev/null || { echo "this host has no ow-host; replace it with a newer release first"; return 1; }
  ow-host "$@"
}

# authorize BOX PUBLIC_KEY: replaces BOX's SSH key list with that key.
authorize() {
  local f
  f=$(mktemp)
  echo "$2" > "$f"
  chmod 644 "$f"
  ow ssh-authorize "$1" --key "$f"
  rm -f "$f"
}

# Before a host replacement: every running box hibernated, then the service
# stopped. `ow down --hibernate` fails (and keeps the boxes running) if a box
# cannot be hibernated, so nothing is stopped then; a bare stop would kill.
hibernate_for_replacement() {
  ow down --hibernate
  systemctl stop open-workspaces
  echo "boxes hibernated; service stopped"
}

# join_tailnet SECRET_ARN HOSTNAME: the key goes to a root-only file under /run
# and is deleted; it is never printed.
join_tailnet() {
  command -v aws > /dev/null || { echo "no AWS CLI on the host (it comes with tailscaleAuthKeySecretArn or backupUrl)"; return 1; }
  command -v tailscale > /dev/null || { echo "Tailscale is not installed (component input tailscaleAuthKeySecretArn)"; return 1; }
  local key
  key=$(mktemp -p /run ow-tailscale.XXXXXX)
  # shellcheck disable=SC2064 # expand now: the trap must remove this file
  trap "rm -f '$key'" EXIT
  # Piped: the snap-packaged aws CLI writes nothing when stdout is a file.
  aws secretsmanager get-secret-value --region "$(cut -d: -f4 <<< "$1")" --secret-id "$1" \
    --query SecretString --output text | cat > "$key" \
    || { echo "could not read the Tailscale auth key from $1"; return 1; }
  [[ -s $key ]] || { echo "the Tailscale auth key came back empty; nothing joined"; return 1; }
  tailscale up --timeout=60s --auth-key="file:$key" --hostname="$2"
  rm -f "$key"
  tailscale ip -4
  restart ""
  # The dashboard (component input dashboardLogins) waits for the tailnet; start it now.
  if systemctl cat open-workspaces-dashboard > /dev/null 2>&1; then
    systemctl restart open-workspaces-dashboard
    sleep 3
    systemctl is-active open-workspaces-dashboard
    tailscale serve status
  fi
}

# build_image NAME BASE64: builds the golden image recipe NAME.sh on the runner.
build_image() {
  ready 3600 images
  local dir
  dir=$(mktemp -d -p /run ow-image.XXXXXX)
  # shellcheck disable=SC2064 # expand now: the trap must remove this directory
  trap "rm -rf '$dir'" EXIT
  base64 -d <<< "$2" > "$dir/$1"
  chown -R ow:ow "$dir"
  ow image build "$dir/$1" 2>&1
}

# backups URL WANT: captures in URL/snapshots (default: the host's backupUrl),
# oldest first, as `uploaded  owner/machine  box  capture`; WANT keeps one box
# (OWNER/NAME or its ID), whose newest capture is then the last line.
backups() {
  local url=$1
  if [[ -z $url ]]; then
    [[ -f /etc/open-workspaces/backup.env ]] || { echo "this host has no backupUrl; pass --url"; return 1; }
    url=$(sed -n 's/^OW_BACKUP_URL=//p' /etc/open-workspaces/backup.env)
  fi
  ow backup list "${url%/}/snapshots" | python3 -c '
import json, sys
want = sys.argv[1]
for row in json.load(sys.stdin):
    if isinstance(row, str):  # before 0.5.0: names only
        row = {"snapshot": row}
    box = row["owner"] + "/" + row["machine"] if row.get("owner") else "-"
    if want in ("", box, row.get("workspace")):
        print("  ".join([row.get("uploaded") or "-", box, row.get("workspace") or "-", row["snapshot"]]))' "$2"
}

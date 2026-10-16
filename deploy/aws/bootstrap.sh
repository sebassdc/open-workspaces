#!/bin/bash
# Open Workspaces host bootstrap, run once per instance as cloud-init user data.
# Never enable xtrace here: the Tailscale auth key passes through this script.
set -euo pipefail
# @OW_ENV@

status=/var/lib/open-workspaces/bootstrap-status
mkdir -p /var/lib/open-workspaces
echo running > "$status"
trap 'echo failed > "$status"' ERR
exec >> /var/log/open-workspaces-bootstrap.log 2>&1
echo "bootstrap start $(date -Is) ref=$OW_REF"
export DEBIAN_FRONTEND=noninteractive

apt-get update -q
apt-get install -y -q --no-install-recommends git curl ca-certificates python3 python-is-python3 \
  build-essential btrfs-progs nftables iproute2 util-linux openssl

tailscale_state=skipped
if [[ -n "$OW_TAILSCALE_SECRET_ARN" ]]; then
  join_tailscale() {
    . /etc/os-release
    curl -fsSL "https://pkgs.tailscale.com/stable/ubuntu/$VERSION_CODENAME.noarmor.gpg" \
      -o /usr/share/keyrings/tailscale-archive-keyring.gpg
    curl -fsSL "https://pkgs.tailscale.com/stable/ubuntu/$VERSION_CODENAME.tailscale-keyring.list" \
      -o /etc/apt/sources.list.d/tailscale.list
    apt-get update -q && apt-get install -y -q tailscale
    snap install aws-cli --classic
    local key
    key=$(mktemp -p /run ow-tailscale.XXXXXX)
    aws secretsmanager get-secret-value --region "$(cut -d: -f4 <<<"$OW_TAILSCALE_SECRET_ARN")" \
      --secret-id "$OW_TAILSCALE_SECRET_ARN" --query SecretString --output text > "$key"
    local joined=0
    tailscale up --timeout=60s --auth-key="file:$key" --hostname="$OW_TAILSCALE_HOSTNAME" ${OW_TAILSCALE_SSH:+--ssh} || joined=$?
    rm -f "$key"
    return "$joined"
  }
  # SSM stays available, so a failed join is reported rather than fatal.
  if join_tailscale; then tailscale_state=joined; else tailscale_state=failed; fi
  echo "tailscale: $tailscale_state"
fi

# The data volume is attached after launch; wait for it, format only if blank.
device="/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_${OW_DATA_VOLUME_ID//-/}"
for _ in $(seq 900); do [[ -e "$device" ]] && break; sleep 1; done
[[ -e "$device" ]] || { echo "data volume $OW_DATA_VOLUME_ID was never attached"; false; }
if ! blkid "$device" >/dev/null; then
  mkfs.btrfs -q -L ow-data "$device"
fi
[[ "$(blkid -s TYPE -o value "$device")" == btrfs ]] || { echo "data volume is not Btrfs; refusing"; false; }
uuid=$(blkid -s UUID -o value "$device")
mkdir -p /srv/ow
grep -q "UUID=$uuid " /etc/fstab || echo "UUID=$uuid /srv/ow btrfs defaults,nofail 0 0" >> /etc/fstab
mountpoint -q /srv/ow || mount /srv/ow

id ow >/dev/null 2>&1 || useradd --system --home-dir /srv/ow --shell /bin/bash --groups kvm ow
chown ow:ow /srv/ow
chmod 700 /srv/ow

# Ubuntu confines unprivileged user namespaces; allow them for the worker only.
cat > /etc/apparmor.d/open-workspaces <<'PROFILE'
abi <abi/4.0>,
include <tunables/global>
profile open-workspaces /srv/ow/src/target/release/ow flags=(unconfined) {
  userns,
  include if exists <local/open-workspaces>
}
PROFILE
apparmor_parser -r /etc/apparmor.d/open-workspaces

runuser -u ow -- env OW_REPOSITORY="$OW_REPOSITORY" OW_REF="$OW_REF" bash -euo pipefail -c '
  cd /srv/ow
  [[ -d src/.git ]] || git clone -q "$OW_REPOSITORY" src
  cd src
  git fetch -q --tags origin
  git checkout -q --detach "origin/$OW_REF" 2>/dev/null || git checkout -q --detach "$OW_REF"
  echo "building $(git rev-parse HEAD)"
  bash scripts/setup-local.sh
'

# `ow host up` keeps controller state under $HOME/.ow; workspace commands use its runner.
cat > /usr/local/bin/ow <<'WRAPPER'
#!/bin/sh
# The worker checks the caller's UID, so every command runs as the ow user.
[ "$(id -un)" = ow ] || exec runuser -u ow -- "$0" "$@"
export HOME=/srv/ow OW_DATA_DIR=/srv/ow/.ow/runner OW_ASSET_DIR=/srv/ow/src/data/runtime-spike
exec /srv/ow/src/target/release/ow "$@"
WRAPPER
chmod 755 /usr/local/bin/ow

# Local runner budget: three quarters of RAM (worker cap 64 GiB), every vCPU, 8 slots.
memory=$(( $(awk '/MemTotal/{print $2}' /proc/meminfo) * 3 / 4 / 1024 ))
(( memory <= 65536 )) || memory=65536
cpus=$(nproc)
(( cpus <= 64 )) || cpus=64
cat > /etc/systemd/system/open-workspaces.service <<UNIT
[Unit]
Description=Open Workspaces controller and local runner (ow host up)
After=network-online.target tailscaled.service
Wants=network-online.target
RequiresMountsFor=/srv/ow

[Service]
User=ow
Environment=HOME=/srv/ow OW_ASSET_DIR=/srv/ow/src/data/runtime-spike
ExecStart=/srv/ow/src/target/release/ow host up --memory $memory --slots 8 --cpus $cpus
ExecStop=/srv/ow/src/target/release/ow --data-dir /srv/ow/.ow/runner down
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now open-workspaces.service

# Done only when the controller reports its local runner online.
runner_online() {
  /usr/local/bin/ow node list | python3 -c 'import json,sys; sys.exit(0 if any(n["online"] for n in json.load(sys.stdin)) else 1)'
}
for _ in $(seq 60); do runner_online 2>/dev/null && break; sleep 2; done
runner_online || { echo "controller never reported its runner online"; journalctl -u open-workspaces -n 30 --no-pager; false; }

# Optional web dashboard: loopback only, served to the tailnet by `tailscale serve`.
# It starts once the host is on the tailnet and retries until then.
if [[ -n "${OW_DASHBOARD_LOGINS:-}" ]]; then
  mkdir -p /etc/open-workspaces /usr/local/lib/open-workspaces
  printf 'OW_DASHBOARD_LOGINS=%s\n' "$OW_DASHBOARD_LOGINS" > /etc/open-workspaces/dashboard.env
  cat > /usr/local/lib/open-workspaces/dashboard-prepare <<'PREPARE'
#!/bin/bash
# Root: write the dashboard config for this host's tailnet name, then turn on `tailscale serve`.
set -euo pipefail
name=$(tailscale status --json 2>/dev/null | python3 -c '
import json, sys
s = json.load(sys.stdin)
sys.exit(1) if s.get("BackendState") != "Running" else print(s["Self"]["DNSName"].rstrip("."))') \
  || { echo "tailscale is not up; the dashboard starts once this host is on the tailnet"; exit 1; }
runuser -u ow -- env NAME="$name" LOGINS="$OW_DASHBOARD_LOGINS" python3 -c '
import json, os
path = "/srv/ow/.ow/dashboard.json"
fd = os.open(path + ".tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump({"auth": "tailscale", "hostname": os.environ["NAME"], "upstream": "http://127.0.0.1:9/",
               "allowed_emails": os.environ["LOGINS"].split(",")}, f)
os.replace(path + ".tmp", path)'
tailscale serve --bg http://127.0.0.1:8787 > /dev/null
echo "dashboard: https://$name/"
PREPARE
  chmod 755 /usr/local/lib/open-workspaces/dashboard-prepare
  cat > /etc/systemd/system/open-workspaces-dashboard.service <<UNIT
[Unit]
Description=Open Workspaces dashboard on loopback, served to the tailnet by tailscale serve
After=open-workspaces.service tailscaled.service
Wants=open-workspaces.service
StartLimitIntervalSec=0

[Service]
User=ow
EnvironmentFile=/etc/open-workspaces/dashboard.env
Environment=HOME=/srv/ow OW_ASSET_DIR=/srv/ow/src/data/runtime-spike
ExecStartPre=+/usr/local/lib/open-workspaces/dashboard-prepare
ExecStart=/srv/ow/src/target/release/ow --data-dir /srv/ow/.ow/controller dashboard --config /srv/ow/.ow/dashboard.json --listen 127.0.0.1:8787
Restart=always
RestartSec=30

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable open-workspaces-dashboard.service
  systemctl start --no-block open-workspaces-dashboard.service
fi

echo "bootstrap done $(date -Is) tailscale=$tailscale_state"
echo "ok tailscale=$tailscale_state" > "$status"

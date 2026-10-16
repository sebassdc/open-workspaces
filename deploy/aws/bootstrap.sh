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
  build-essential btrfs-progs nftables iproute2 util-linux

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

cat > /usr/local/bin/ow <<'WRAPPER'
#!/bin/sh
# The worker checks the caller's UID, so every command runs as the ow user.
[ "$(id -un)" = ow ] || exec runuser -u ow -- "$0" "$@"
export OW_DATA_DIR=/srv/ow/workspaces OW_ASSET_DIR=/srv/ow/src/data/runtime-spike
exec /srv/ow/src/target/release/ow "$@"
WRAPPER
chmod 755 /usr/local/bin/ow

cat > /etc/systemd/system/open-workspaces.service <<'UNIT'
[Unit]
Description=Open Workspaces controller and runner
After=network-online.target
Wants=network-online.target
RequiresMountsFor=/srv/ow

[Service]
Type=oneshot
RemainAfterExit=yes
User=ow
Environment=OW_DATA_DIR=/srv/ow/workspaces OW_ASSET_DIR=/srv/ow/src/data/runtime-spike
ExecStart=/srv/ow/src/target/release/ow up
ExecStop=/srv/ow/src/target/release/ow down
TimeoutStartSec=120

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now open-workspaces.service

echo "bootstrap done $(date -Is) tailscale=$tailscale_state"
echo "ok tailscale=$tailscale_state" > "$status"

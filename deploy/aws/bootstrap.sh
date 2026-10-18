#!/bin/bash
# Open Workspaces host stage 0, run once per instance as cloud-init user data:
# packages, the data volume, the tailnet state mount, the ow user and the config
# (an SSM parameter), then the configured release's own `ow-host boot`. Everything
# else (units, Tailscale join, dashboard, images, backups) lives in the release,
# so a config or release change never changes this script or replaces the host.
# Never enable xtrace here.
set -euo pipefail
# @OW_ENV@

status=/var/lib/open-workspaces/bootstrap-status
mkdir -p /var/lib/open-workspaces
echo running > "$status"
trap 'echo failed > "$status"' ERR
exec >> /var/log/open-workspaces-bootstrap.log 2>&1
echo "bootstrap start $(date -Is) stage0=$OW_STAGE0 config=$OW_CONFIG_PARAMETER"
export DEBIAN_FRONTEND=noninteractive

apt-get update -q
apt-get install -y -q --no-install-recommends git curl ca-certificates python3 python-is-python3 \
  build-essential btrfs-progs nftables iproute2 util-linux openssl unzip

# AWS CLI v2 (the snap build refuses the ow user's home outside /home): the config,
# the Tailscale key and backups go through it.
if [[ ! -x /usr/local/bin/aws ]]; then
  tmp=$(mktemp -d)
  curl -fsSL https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip -o "$tmp/awscli.zip"
  unzip -q "$tmp/awscli.zip" -d "$tmp"
  "$tmp/aws/install"
  rm -rf "$tmp"
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

# The tailnet identity lives on the data volume, so a replaced host (new AMI or
# stage 0) comes back as the same device and name instead of a new `-1` device.
# Root-only, bind-mounted before tailscaled is installed or started.
mkdir -p /srv/ow/.tailscale-state /var/lib/tailscale
chown root:root /srv/ow/.tailscale-state
chmod 700 /srv/ow/.tailscale-state
grep -q " /var/lib/tailscale none bind" /etc/fstab \
  || echo "/srv/ow/.tailscale-state /var/lib/tailscale none bind,nofail,x-systemd.requires-mounts-for=/srv/ow 0 0" >> /etc/fstab
mountpoint -q /var/lib/tailscale || mount /var/lib/tailscale
mkdir -p /etc/systemd/system/tailscaled.service.d
printf '[Unit]\nRequiresMountsFor=/var/lib/tailscale\n' > /etc/systemd/system/tailscaled.service.d/open-workspaces-state.conf
systemctl daemon-reload

id ow >/dev/null 2>&1 || useradd --system --home-dir /srv/ow --shell /bin/bash --groups kvm ow
chown ow:ow /srv/ow
chmod 700 /srv/ow
install -d -o ow -g ow -m 700 /srv/ow/.ow

# The config: fetched now, kept on the data volume as the last good copy for a boot
# without SSM. Retried: the host role's policy may still be propagating.
mkdir -p /etc/open-workspaces
printf 'OW_CONFIG_PARAMETER=%s\nOW_REGION=%s\n' "$OW_CONFIG_PARAMETER" "$OW_REGION" > /etc/open-workspaces/host.env
config=/srv/ow/.ow/host-config.json
fetched=
for _ in $(seq 30); do
  if aws ssm get-parameter --region "$OW_REGION" --name "$OW_CONFIG_PARAMETER" \
    --query Parameter.Value --output text > "$config.new" && [[ -s $config.new ]]; then
    mv "$config.new" "$config"
    fetched=1
    break
  fi
  sleep 10
done
rm -f "$config.new"
[[ -n $fetched ]] || [[ -s $config ]] || { echo "no config: $OW_CONFIG_PARAMETER unreadable and no stored copy"; false; }
[[ -n $fetched ]] || echo "using the stored config: $OW_CONFIG_PARAMETER unreadable"
chmod 600 "$config"

# The release's sources: /srv/ow/src is the clone every release directory is a
# worktree of. ow-host boot (from the configured ref) builds and starts it.
read -r repository ref < <(python3 -c 'import json,sys; r=json.load(open(sys.argv[1]))["release"]; print(r["repository"], r["ref"])' "$config")
runuser -u ow -- env OW_REPOSITORY="$repository" OW_REF="$ref" bash -euo pipefail -c '
  cd /srv/ow
  [[ -d src/.git ]] || git clone -q "$OW_REPOSITORY" src
  cd src
  git remote set-url origin "$OW_REPOSITORY"
  git fetch -q --tags --force origin
  git checkout -q --detach "origin/$OW_REF" 2>/dev/null || git checkout -q --detach "$OW_REF"
  echo "release $OW_REF at $(git rev-parse HEAD)"
'
python3 -I /srv/ow/src/deploy/aws/host/ow_host.py boot
echo "bootstrap done $(date -Is)"

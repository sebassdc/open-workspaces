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
# ref) comes back as the same device and name instead of a new `-1` device.
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
    # Piped: the snap-packaged aws CLI writes nothing when its stdout is a regular file.
    # An empty key would make `tailscale up` fall back to an interactive login.
    if ! aws secretsmanager get-secret-value --region "$(cut -d: -f4 <<<"$OW_TAILSCALE_SECRET_ARN")" \
      --secret-id "$OW_TAILSCALE_SECRET_ARN" --query SecretString --output text | cat > "$key" \
      || [[ ! -s "$key" ]]; then
      echo "tailscale: no auth key in the secret; not joining"
      rm -f "$key"
      return 1
    fi
    local joined=0
    # Explicit --ssh=true|false: state kept from an earlier host must not keep an old setting.
    tailscale up --timeout=60s --auth-key="file:$key" --hostname="$OW_TAILSCALE_HOSTNAME" \
      --ssh="$([[ -n $OW_TAILSCALE_SSH ]] && echo true || echo false)" || joined=$?
    rm -f "$key"
    return "$joined"
  }
  # SSM stays available, so a failed join is reported rather than fatal.
  if join_tailscale; then tailscale_state=joined; else tailscale_state=failed; fi
  echo "tailscale: $tailscale_state"
fi

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
# `ow host up` picks its listen address once, at start: the tailnet address when
# Tailscale is up, else loopback. At boot tailscaled is often still connecting,
# so wait (up to 60 s) for it to be Running; a host that is not joined starts at once.
mkdir -p /usr/local/lib/open-workspaces
cat > /usr/local/lib/open-workspaces/wait-tailnet <<'WAIT'
#!/bin/sh
command -v tailscale > /dev/null || exit 0
i=0
while [ "$i" -lt 60 ]; do
  state=$(tailscale status --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("BackendState",""))' 2>/dev/null)
  case $state in
    Running) tailscale ip -4 > /dev/null 2>&1 && exit 0 ;;
    NeedsLogin|NeedsMachineAuth|Stopped) exit 0 ;;
  esac
  i=$((i + 1))
  sleep 1
done
echo "tailnet not up after 60 s; the controller listens on loopback until the next restart"
WAIT
chmod 755 /usr/local/lib/open-workspaces/wait-tailnet
# A service stop (restart, reboot, instance stop or replacement) hibernates the
# running boxes before `ow down`, so their processes survive; they wake on the
# next connection. A box that fails to hibernate is cold-stopped by `ow down`.
cat > /usr/local/lib/open-workspaces/stop <<'STOP'
#!/bin/sh
running() { /usr/local/bin/ow list | python3 -c 'import json,sys; print(" ".join(w["id"] for w in json.load(sys.stdin) if w["state"] == "running"))'; }
# A connection can wake a box while others are being captured, so go round
# until a listing shows none running (at most five rounds).
for _ in 1 2 3 4 5; do
  boxes=$(running) || break
  [ -n "$boxes" ] || break
  for box in $boxes; do
    /usr/local/bin/ow hibernate "$box" > /dev/null && echo "hibernated $box" || echo "could not hibernate $box"
  done
done
exec /srv/ow/src/target/release/ow --data-dir /srv/ow/.ow/runner down
STOP
chmod 755 /usr/local/lib/open-workspaces/stop
cat > /etc/systemd/system/open-workspaces.service <<UNIT
[Unit]
Description=Open Workspaces controller and local runner (ow host up)
After=network-online.target tailscaled.service
Wants=network-online.target
RequiresMountsFor=/srv/ow

[Service]
User=ow
Environment=HOME=/srv/ow OW_ASSET_DIR=/srv/ow/src/data/runtime-spike
ExecStartPre=/usr/local/lib/open-workspaces/wait-tailnet
ExecStart=/srv/ow/src/target/release/ow host up --memory $memory --slots 8 --cpus $cpus
ExecStop=/usr/local/lib/open-workspaces/stop
# Hibernating hashes each box's RAM and disk (about 25 s for 1 GiB and 8 GiB).
TimeoutStopSec=900
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

# Optional developer image (input developerImages): the `dev` user, SSH server
# and toolchains that SSH access and golden-image recipes build on. Built once
# per data volume in a disposable microVM, in the background (10-20 minutes),
# then published under the built-in profile name; retried at boot until done.
if [[ -n "${OW_DEVELOPER_IMAGES:-}" ]]; then
  cat > /usr/local/lib/open-workspaces/developer-images <<'IMAGES'
#!/bin/bash
set -euo pipefail
cd /srv/ow/src
guest=data/runtime-spike/guest
for profile in "$@"; do
  if python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("developer_user") else 1)' "$guest/$profile.json" 2>/dev/null; then
    echo "$profile: developer image present"
    continue
  fi
  if [[ ! -f $guest/$profile.ext4 ]]; then
    rm -rf "$guest/$profile.tmp" "$guest"/build-"$profile"-*
    # Its mkfs runs in a user namespace, which Ubuntu's AppArmor policy allows only
    # under the open-workspaces profile.
    aa-exec -p open-workspaces -- python3 experiments/runtime-spike/prepare-profiles.py "$profile"
  fi
  mkdir -p data/dev-images
  out=data/dev-images/out-$profile-$(date +%s)
  python3 experiments/guest-dev/build.py "$profile" --output-assets "$out"
  # Boxes clone the image when created, so replacing it leaves existing boxes alone.
  mv "$out/guest/$profile.ext4" "$guest/$profile.ext4"
  mv "$out/guest/$profile.json" "$guest/$profile.json"
  rm -rf "$out"
  for build in data/dev-images/build-*; do [[ -f $build/result.json ]] && rm -rf "$build"; done
  echo "$profile: developer image built"
  touch /srv/ow/.ow/developer-images-new
done
IMAGES
  chmod 755 /usr/local/lib/open-workspaces/developer-images
  cat > /etc/systemd/system/open-workspaces-images.service <<UNIT
[Unit]
Description=Open Workspaces developer images ($OW_DEVELOPER_IMAGES), built once
After=open-workspaces.service

[Service]
Type=oneshot
User=ow
Environment=HOME=/srv/ow OW_ASSET_DIR=/srv/ow/src/data/runtime-spike
ExecStart=/usr/local/lib/open-workspaces/developer-images $OW_DEVELOPER_IMAGES
# The runner advertises its images when it connects: restart it after a new build
# (running boxes hibernate first, through the service's stop hook).
ExecStartPost=+/bin/sh -c 'if [ -e /srv/ow/.ow/developer-images-new ]; then rm /srv/ow/.ow/developer-images-new; systemctl restart open-workspaces; fi'
TimeoutStartSec=3600

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable open-workspaces-images.service
  systemctl start --no-block open-workspaces-images.service
fi

echo "bootstrap done $(date -Is) tailscale=$tailscale_state"
echo "ok tailscale=$tailscale_state" > "$status"

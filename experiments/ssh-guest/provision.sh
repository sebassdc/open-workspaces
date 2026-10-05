#!/bin/sh
# Guest-only, explicit image build or opt-in upgrade. Never execute on host.
set -eu
profile=$(cat /etc/ow-image)
case "$profile" in
 ubuntu)
  export DEBIAN_FRONTEND=noninteractive
  printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
  chmod 755 /usr/sbin/policy-rc.d
  apt-get update
  apt-get install -y --no-install-recommends openssh-server procps
  ;;
 arch) pacman -S --noconfirm --needed openssh procps-ng ;;
 *) echo 'Only Ubuntu/Arch guest SSH profiles supported' >&2; exit 1 ;;
esac
id dev >/dev/null
# Locked accounts fail publickey with UsePAM=no; a non-password sentinel allows
# key authentication while PasswordAuthentication=no remains mandatory.
usermod -p '*' dev
mkdir -p /etc/ow-ssh /run/sshd
chmod 700 /etc/ow-ssh
cat > /etc/ow-ssh/sshd_config <<'CONFIG'
Port 22
AddressFamily inet
HostKey /etc/ssh/ssh_host_ed25519_key
PidFile /run/ow-sshd.pid
AuthorizedKeysFile /etc/ow-ssh/authorized_keys
StrictModes yes
AllowUsers dev
PermitRootLogin no
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitEmptyPasswords no
PubkeyAuthentication yes
AuthenticationMethods publickey
UsePAM no
AllowAgentForwarding no
AllowTcpForwarding local
AllowStreamLocalForwarding no
GatewayPorts no
PermitTunnel no
X11Forwarding no
MaxAuthTries 3
MaxSessions 8
MaxStartups 4:30:16
LoginGraceTime 20
ClientAliveInterval 20
ClientAliveCountMax 3
Subsystem sftp internal-sftp
CONFIG
chmod 600 /etc/ow-ssh/sshd_config
printf '1\n' > /etc/ow-ssh-v1
# No enrolled keys or generated identity may enter a new template.
rm -f /etc/ow-ssh/authorized_keys /etc/ssh/ssh_host_*
sync

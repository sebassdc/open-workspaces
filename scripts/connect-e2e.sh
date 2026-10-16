#!/bin/sh
# Real OpenSSH through the ssh://, ssm:// and tailscale:// connect providers, inside a
# throwaway Linux container: sshd on loopback, with stand-ins for `aws ssm
# start-session` and `tailscale nc` that check their arguments and then tunnel to it.
# Usage (from the repository root): scripts/connect-e2e.sh [rust image]
set -eu
image=${1:-rust:1.97.0}
exec docker run --rm -v "$PWD":/src:ro -e CARGO_TARGET_DIR=/tmp/target -w /src "$image" sh -euc '
apt-get -qq update >/dev/null && apt-get -qq install -y openssh-server netcat-openbsd >/dev/null 2>&1
mkdir -p /run/sshd /root/.ssh /tmp/bin && chmod 700 /root/.ssh
ssh-keygen -q -t ed25519 -N "" -f /root/.ssh/id_ed25519
cp /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys
printf "StrictHostKeyChecking no\nUserKnownHostsFile /dev/null\nLogLevel ERROR\n" > /root/.ssh/config
/usr/sbin/sshd -o ListenAddress=127.0.0.1 -o PermitRootLogin=prohibit-password
cat > /tmp/bin/aws <<"AWS"
#!/bin/sh
[ "$*" = "ssm start-session --target i-0123456789abcdef0 --document-name AWS-StartSSHSession --parameters portNumber=22" ] || { echo "unexpected aws $*" >&2; exit 9; }
exec nc 127.0.0.1 22
AWS
cat > /tmp/bin/tailscale <<"TS"
#!/bin/sh
[ "$*" = "nc build-1.tail1234.ts.net 22" ] || { echo "unexpected tailscale $*" >&2; exit 9; }
exec nc 127.0.0.1 22
TS
chmod +x /tmp/bin/aws /tmp/bin/tailscale
PATH=/tmp/bin:$PATH cargo test -q -p ow-core -- --ignored real_openssh_over_each_transport
'

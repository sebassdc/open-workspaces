#!/bin/bash
# Runs inside a disposable guest build VM, never on the host.
set -euo pipefail
profile=${1:?ubuntu or arch}
[ -f /etc/ow-image ] && [ "$(cat /etc/ow-image)" = "$profile" ] || { echo 'Guest image marker mismatch'; exit 1; }
[ "$(id -u)" = 0 ] || { echo 'Guest root required'; exit 1; }
chmod 1777 /tmp /var/tmp
export DEBIAN_FRONTEND=noninteractive
if [ "$profile" = ubuntu ]; then
  chmod 755 /etc/ssl /etc/ssl/certs /ow /ow/bin /ow/lib /ow/etc
  chmod 644 /etc/ssl/certs/ca-certificates.crt
  sed -i 's|http://archive.ubuntu.com|https://archive.ubuntu.com|;s|http://security.ubuntu.com|https://security.ubuntu.com|' /etc/apt/sources.list.d/ubuntu.sources
  printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
  chmod 755 /usr/sbin/policy-rc.d
  apt-get update
  apt-get install -y --no-install-recommends ca-certificates curl wget git neovim build-essential pkg-config cmake ninja-build libssl-dev libffi-dev zlib1g-dev libbz2-dev libreadline-dev libsqlite3-dev liblzma-dev xz-utils unzip zip jq ripgrep fd-find fzf tmux less man-db bash-completion sudo openssh-client locales gnupg
  ln -sfn /usr/bin/fdfind /usr/local/bin/fd
  sed -i 's/^# en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen
  locale-gen
elif [ "$profile" = arch ]; then
  # Regular signed core/extra repositories; refresh the keyring before a full upgrade.
  pacman-key --init
  pacman-key --populate archlinux
  pacman -Sy --noconfirm archlinux-keyring
  pacman -Syu --noconfirm --needed base-devel ca-certificates curl wget git neovim pkgconf cmake ninja openssl libffi zlib bzip2 readline sqlite xz unzip zip jq ripgrep fd fzf tmux less man-db bash-completion sudo openssh gnupg
  sed -i 's/^#en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen
  locale-gen
else
  exit 1
fi
# Dedicated guest account. No copied host account, password, credentials or dotfiles.
id dev >/dev/null 2>&1 || useradd -m -u 1000 -s /bin/bash dev
passwd -l dev
mkdir -p /etc/sudoers.d
printf 'dev ALL=(ALL:ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/90-ow-dev
chmod 440 /etc/sudoers.d/90-ow-dev
visudo -cf /etc/sudoers
mkdir -p /home/dev/.config/mise /persist
chown dev:dev /home/dev /home/dev/.config /home/dev/.config/mise /persist
# Official pinned mise executable verified before installation.
curl --proto '=https' -fsSL https://github.com/jdx/mise/releases/download/v2026.10.0/mise-v2026.10.0-linux-x64 -o /usr/local/bin/mise.staging
printf '57ced973f968b8fbab07aa8e32bd7077d4a357e200a22356d98963c723c6de0a  /usr/local/bin/mise.staging\n' | sha256sum --check --status
chmod 755 /usr/local/bin/mise.staging
mv /usr/local/bin/mise.staging /usr/local/bin/mise
cat > /home/dev/.bashrc <<'RC'
export LANG=en_US.UTF-8
export TERM=xterm-256color
export EDITOR=nvim
export VISUAL=nvim
export PS1='\u@\h:\w\$ '
alias ll='ls -alF'
if [ -f /usr/share/bash-completion/bash_completion ]; then . /usr/share/bash-completion/bash_completion; fi
if [ -f /etc/bash_completion ]; then . /etc/bash_completion; fi
eval "$(mise activate bash)"
RC
printf '[ -f ~/.bashrc ] && . ~/.bashrc\n' > /home/dev/.bash_profile
chown dev:dev /home/dev/.bashrc /home/dev/.bash_profile
# Resolve and install during build, then record exact installed versions.
su - dev -c 'mise use --global node@24.21.0 python@3.13.16'
su - dev -c 'mise use --global --tool-option profile=minimal --tool-option components=rustfmt,clippy rust@1.99.0'
su - dev -c 'mise exec -- node --version; mise exec -- python --version; mise exec -- cargo --version; mise exec -- rustc --version'
printf 'dev\n' > /etc/ow-developer
mkdir -p /etc/ow-dev
if [ "$profile" = ubuntu ]; then dpkg-query -W > /etc/ow-dev/packages.txt; apt-get clean; rm -rf /var/lib/apt/lists/*; else pacman -Q > /etc/ow-dev/packages.txt; rm -f /var/cache/pacman/pkg/*.pkg.tar.*; fi
su - dev -c 'mise ls --json' > /etc/ow-dev/toolchains.json
printf 'ow developer image v1; guest-only passwordless sudo; mise 2026.10.0\n' > /etc/ow-dev/version
# Remove builder identity and histories. Runtime sets a fresh identity when starting.
rm -f /root/.bash_history /home/dev/.bash_history /etc/machine-id /etc/ssh/ssh_host_*
sync

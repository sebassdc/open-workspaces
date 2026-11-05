# Install a released remote client

GitHub Releases are the canonical source of compiled `ow` clients. Pick an exact
published version containing the assets below. A tag, a source archive or a
successful source build alone is not binary delivery. Older v0.7.0–v0.7.3 releases
have no client assets. v0.8.0 is the coordinated release candidate for this path;
check that it has been published before using the example.

## Pinned user install

This example pins **every download** to `v0.8.0`, including the installer and its
checksum. A consuming project should substitute its own release-lock version;
do not use `/releases/latest/download` or a script from the default branch.
Run these commands in a shell with curl, standard POSIX utilities, and either
`sha256sum` (Linux) or `shasum` (macOS):

```sh
set -eu
version=v0.8.0
base="https://github.com/sebassdc/open-workspaces/releases/download/$version"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT HUP INT TERM
curl --proto '=https' --tlsv1.2 -fsSL "$base/install.sh" -o "$staging/install.sh"
curl --proto '=https' --tlsv1.2 -fsSL "$base/SHA256SUMS" -o "$staging/SHA256SUMS"
(
  cd "$staging"
  grep '  install.sh$' SHA256SUMS > installer.sha256
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c installer.sha256
  else
    shasum -a 256 -c installer.sha256
  fi
)
sh "$staging/install.sh" --version "$version"
```

The installer selects the current operating system and CPU, downloads that
version's binary and `.sha256`, checks its checksum and executable platform,
and runs `ow --version` before replacing anything. It stages on the destination
filesystem and atomically replaces a regular installed client only after all
checks succeed. Missing, corrupted, wrong-platform or wrong-version downloads
leave an existing client intact. Symlink/directory targets are rejected. Repeating
the same install is safe. Checksums detect corruption; trust comes from the
selected repository/release and HTTPS, not a separate signing authority.

Default destination: `~/.local/bin/ow`. Set `OW_INSTALL_DIR` to another user-owned
directory if needed. Follow the printed PATH command, for example:

```sh
export PATH="$HOME/.local/bin:$PATH"
ow --version
ow login --tailnet https://NAME.TAILNET.ts.net
ow list
ow exec MACHINE -- 'uname -a'
```

The installer never edits shell profiles. Add the PATH line yourself to your
shell's configuration if you want it to persist. Installing/running the client
requires no Rust, Cargo, root or cloudflared for tailnet login. Your computer must
already be admitted to the host's tailnet as your own user; installation does
not enroll the device or change access policy. The endpoint comes from the host
operator or your project's connection guide.

For a Cloudflare Access host, pass `--access` after the version. This retains the
pinned, checksum-verified cloudflared helper used by the existing installer:

```sh
sh "$staging/install.sh" --version "$version" --access
ow login https://YOUR_ACCESS_HOST
```

Access mode requires browser sign-in. It does not require Cloudflare administrator
credentials. Existing host-served `/cli/install.sh` installations remain compatible;
see [networking and remote CLI](NETWORKING.md). They are a compatibility path,
not the versioned GitHub Release download path.

## Keeping the CLI current

`ow upgrade` replaces the installed client with the newest published release
that works with your controller. It finds that release from the redirect of
`https://github.com/sebassdc/open-workspaces/releases/latest` (GitHub sends it
only to a published release that is not a draft or prerelease), reading the tag
from the `Location` header without following it, so a timer spends no GitHub API
quota. It downloads this
platform's executable from that GitHub release and checks it as the installer
does: the digest must match the asset's `.sha256`, the release's `SHA256SUMS`
must list that digest for the asset, the header must be this platform's
executable, and the new binary must report the expected version. The new binary
then runs its own `ow status` against the same HTTPS server (the saved login, or
the `--server` this command got) within 30 seconds; it must report
`"connected": true` and `"compatibility": {"next_action": null}`. A controller
reached over this host's local socket (`--local`, or `--data-dir` without
`--server`) cannot check that, so there `ow upgrade` targets the controller's
release.
Only then is it renamed over the running client, keeping its permissions. Any
failed check leaves the current client in place.

If the latest release cannot be found, its assets are not all published yet,
a check fails or its `status` does, `ow upgrade` falls back to the release the
controller runs (`GET /api/version`, which can lag the latest release) and says
why. A latest release that fails any step after its `.sha256` was found is not
downloaded again for 10 minutes (recorded in `upgrade-refused` beside the
client's `client.json`), and that refusal prints one line to standard error, so
a timer's log shows it even with `--json`. Small requests (the latest lookup,
`.sha256`, `SHA256SUMS`) time out after 10 seconds; the client download after 5
minutes. It refuses, before downloading anything, when the client was started
through a symlink (as the installer does), when it cannot write to the client's
directory, and when it is a host's own release (a file in a host's `releases/`
directory, which the host's services run): upgrade a host with `ow-host upgrade`. A newer latest release is only downloaded when the
controller answers `GET /api/version`, since its compatibility cannot be checked
otherwise.

```sh
ow upgrade                  # to the latest compatible release; never older than this client
ow upgrade --check          # print current and target; exit 0 if current, 10 if an upgrade is available
ow upgrade --version v0.9.58  # exactly this release, even an older one; no latest lookup or status check
ow upgrade --json           # after an upgrade: {"from":"0.9.62","to":"0.9.64","seconds":2.4,"upgraded":true,"source":"latest"}
```

`source` says where the target came from: `latest`, `controller` (the
fallback, with a `reason`) or `version`. An equal or older latest release prints
one line and exits 0 without downloading anything; with `--json` it prints
nothing, so a timer's log grows only when something changed (`--check --json`
always prints the line). Only an explicit `--version` installs an older release.
`--check` downloads no client, so it reports the latest release with the
`reason` that its compatibility is checked on install; it exits 10 only once
the release's `.sha256` for this platform can be downloaded. Errors exit 1.

To keep a Mac's client current, run it from a launchd user
agent every 30 seconds. Save this as
`~/Library/LaunchAgents/dev.open-workspaces.upgrade.plist`, then load it with
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.open-workspaces.upgrade.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.open-workspaces.upgrade</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>exec "$HOME/.local/bin/ow" upgrade --json >> "$HOME/Library/Logs/ow-upgrade.log" 2>&amp;1</string>
  </array>
  <key>StartInterval</key>
  <integer>30</integer>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
```

Adjust the client path if you installed it with `OW_INSTALL_DIR`. A Cloudflare
Access login needs a current browser session for the check to reach the
controller; a tailnet login needs nothing more.

## Assets and runtime acceptance

Each new release contains the following raw executables, each with a `.sha256`
file containing its lowercase SHA-256 digest:

- `ow-darwin-arm64`: native Apple Silicon macOS client.
- `ow-darwin-amd64`: native Intel macOS client.
- `ow-linux-amd64`: static musl Linux x86-64 client.

The release also contains `install.sh`, `SHA256SUMS` (filename-bearing checksums
for the installer, executables, per-executable checksum files and source record),
and `source-commit.txt` (the exact Git commit used by the workflow). Compare that
source record with the resolved release tag or a consumer's pinned commit when
checking provenance. Never copy a binary from another revision into a release.
No SDK, build tool, guest image, private connection configuration or credentials
are included.

macOS jobs target macOS 13.0 and execute the resulting client on macOS 15 Apple
Silicon; the Intel client is cross-built there and executed under Rosetta 2, not
on Intel hardware. macOS 15+ is the supported release-client baseline;
13/14 execution has not been accepted even though the build deployment floor is
13.0. The binaries are not Apple-notarized. Linux jobs execute the static client
on Ubuntu 24.04 and, as an unprivileged user, in Alpine 3.22 without host libraries.
Other Linux distributions/kernel versions are unverified. Linux ARM, Windows,
32-bit systems and macOS releases below that supported baseline are unsupported
by this installer. A compiled client does not make its platform a VM host.

## Release checks

The existing `aws-component-release` workflow builds clients from the same commit
it tags. It reuses `scripts/build-local.sh` for Linux and `scripts/build-macos.sh`
on native Mac runners; the latter's Linux cross-build path is retained. All three
jobs run a fresh user-directory install and repeat it with Cargo, sudo and
cloudflared absent from PATH. They also exercise failed downloads, checksum,
platform/version rejection and the Access compatibility path. These are fixture
downloads of the real built binary, separate from live GitHub download acceptance.

Builds, runtime fixtures and existing release checks must pass before tagging.
The workflow collects verified outputs, creates a draft release, attaches all
assets and then publishes it. Retries preserve identical existing assets and fail
on differing bytes rather than replacing them. Existing tags must name the exact
commit; choose a fresh version if a tag is occupied. A failed upload leaves a draft
for diagnosis/retry; a draft is not a completed published release.

After publication, the workflow starts a fresh job on each supported runner. It
downloads the published installer and checksum manifest over HTTPS, verifies the
source record against the exact workflow commit, installs the actual release twice
in a clean user directory and checks its checksum and expected version. Those jobs
must pass separately from build-time fixture checks. Acceptance still requires
connecting through the existing tailnet login as an admitted user. Record that
evidence separately for each tested platform/runtime: version execution does not
prove user admission, TLS trust or a live authenticated connection. Consumer release
pins and connection guides are maintained in the consuming project, after this
evidence.

Once the release is published, the workflow can tell downstream repositories at
once. Set the repository secret `RELEASE_DISPATCH_REPOS` to a list of
`owner/repo` names (spaces or one per line; a secret, so the names stay out of the public logs) and the secret `RELEASE_DISPATCH_TOKEN` to a token
allowed to send `repository_dispatch` events to them (contents write on each).
Each listed repository then receives event type `open-workspaces-release` with
`client_payload` `{"tag": "vX.Y.Z", "commit": "<sha>", "version": "X.Y.Z"}`,
for example to start its own deployment from a
`on: repository_dispatch: types: [open-workspaces-release]` workflow. When
either setting is unset the step logs one line and succeeds; a failed dispatch
is a warning and never fails the release.

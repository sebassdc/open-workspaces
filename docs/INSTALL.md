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

Native macOS jobs target macOS 13.0 and execute the resulting client on macOS 15
(Apple Silicon and Intel). macOS 15+ is the supported release-client baseline;
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

# Browser dashboard

The dashboard supports Tailscale identity with an operator-configured
[capability grant](#admission-through-a-capability-grant), or an explicitly
configured Cloudflare Access JWT gate. It serves the embedded browser UI and
owner-scoped API. Use [tailnet sign-in](#tailnet-sign-in) for the current tailnet
setup, [users and sharing](USERS.md) for authorization, and
[previews](DAILY_USE.md#tailnet-https-previews-v080) for separate guest app URLs.

## Original Access deployment (historical)

The owner requested a dashboard on the **existing project hostname**, replacing
its guest counter demo. No additional hostname or domain was created. An
HTTP handler serves embedded HTML/CSS/JavaScript and a scoped API behind the
existing Cloudflare Access application and tunnel. Infrastructure identifiers,
user emails, tokens and deployment logs remain in ignored private storage.

## Implemented experience

- List existing machines and select one; create 256/512/1024 MiB Linux machines
  with Alpine, headless Arch or Ubuntu Base userspace. See [image profiles](GUEST_IMAGES.md).
- Start/resume, hibernate with memory+disk retained, and stop with disk retained.
- Capture named paired snapshots, fork independently from an existing snapshot
  or a new live capture, and restore a checkpoint to its original machine.
- Run guest commands, show output and actual exit status, keep per-machine
  browser-session command history, and render guest output strictly as text.
- View measured process memory, running count, reservations and the worker's
  4 GiB reservation cap. Hibernation releases running VM reservations; stored
  disk/snapshot data and host cache still consume resources.
- Open a real interactive guest PTY over authenticated WebSocket, with terminal
  colors, persistent shell state, Ctrl-C, job control, resizing and interactive
  editors. The browser uses locally vendored xterm.js.
- Desktop and mobile layouts, operation progress/errors and polling updates.
  Restore and stop explain memory/disk consequences before the user's action.

The command console invokes `/bin/sh -c` for each command, with output after
completion. A command gets 30 s; after that the guest stops it (TERM, then
KILL to its process group), the console says so, and the next command runs at
once and shows only its own output. **Open Terminal** creates a separate
interactive PTY inside the selected running VM. See [terminal transport and limits](TERMINALS.md).
Guest networking now supports filtered IPv4 Internet egress. The page includes a copyable curl installer for the remote Linux x86-64 CLI; see [networking and CLI](NETWORKING.md). **Rename** (owner only, any state)
changes only the machine's name and keeps it selected; its disk, snapshots,
shares, previews and running state stay. **Delete** (owner only, on a
stopped or hibernated machine) removes it with its disk and every snapshot taken
from it, and closes the sessions of anyone it was shared with. A machine its
runner no longer lists shows as `missing` and holds no capacity; deleting it
only drops the entry. **AWS role** (owner or admin, when the operator configured
[per-box AWS access](AWS_ACCESS.md)) attaches one role from the operator's
allow-list; the machine details then show it with its token expiry and a
**Detach** button. The configured pool administrator also sees other owners' box
names and AWS settings in the host controls, with attach/detach actions that need
no share grant; their role choices still follow each box owner's allow-list.
These settings grant no terminal or disk access. The API does not delete snapshots on their own, change host configuration, grant Cloudflare permissions, or serve
arbitrary guest HTML on the dashboard origin. Public guest-app publishing remains
a subsequent slice.

## Access and API boundaries

`./ow dashboard --config PRIVATE_CONFIG --listen 127.0.0.1:8787` uses the existing
Access JWT verification: RS256 signature, issuer, application audience, expiry,
not-before, subject and the explicit email list. It accepts only the configured
Host and loopback listener. API/static assets require the same authentication.
The legacy config's `upstream` remains for compatibility with `ow serve`; the
`dashboard` command does not forward requests to that guest upstream.

Every email admitted by the current app/gateway list is an **operator of the
whole local sandbox**. This is one operator model, not per-workspace roles,
organizations or multi-tenant authorization. The current deployment allows only
the owner and uses the existing Cloudflare login provider. Adding non-account
users through email PIN requires preserving other apps' login behavior first.

- `GET /api/state` returns workspace summaries, snapshot summaries and memory
  measurements. It omits worker/hypervisor socket paths and worker PID details.
  Machines carry `created_at`; a live fork's own capture names its child as
  `fork`; a failed capture names its machine (`workspace`, null once deleted)
  and `failed_at`. Times are Unix seconds.
- `POST /api/operation` accepts only create/start/stop/hibernate/snapshot/fork/
  restore/exec, with typed fields, strict unknown-field rejection, validated IDs,
  bounded memory profiles, 2,000-byte commands and 16 KiB operation bodies.
- `GET /api/terminal/ID` upgrades to WebSocket only after JWT, Host and exact
  HTTPS Origin checks. It opens only a PTY in that running workspace; the client
  cannot choose a host socket, network address, port or arbitrary worker request.
- Mutations require the exact HTTPS Origin, `application/json` and a dashboard
  request header. There is no CORS grant. Raw worker operation forwarding,
  shutdown, tunnels and host file operations are not available through this API.
- A same-owner Unix control connection dispatches work inside the dedicated
  worker namespace. Guest commands execute inside the selected VM, never in a
  host shell. The HTTP request cannot choose another worker data directory.
- CSP permits local scripts/connections; inline styles are allowed for xterm
  rendering, while inline scripts, frames and base URL changes are forbidden. Guest names/output use DOM text nodes. Responses
  disable caching and framing; worker diagnostics containing host paths stay in
  private operator logs rather than API responses.

The underlying worker still serializes control operations. HTTP timeout does not
cancel an already queued/running VM operation; refresh state before retrying a
failed request, especially exec. Broader jailer/cgroup and worker recovery gates
remain open. There is no production hardening or multi-tenant claim.

## Tailnet sign-in

Instead of Cloudflare Access, the dashboard can trust the tailnet login that
[`tailscale serve`](https://tailscale.com/kb/1312/serve) adds to each proxied
request. The dashboard stays on loopback; `tailscale serve` (not `funnel`)
publishes it to the tailnet only, with a tailnet HTTPS certificate.

```json
{"auth": "tailscale", "hostname": "NAME.TAILNET.ts.net",
 "allowed_emails": ["you@example.com"], "upstream": "http://127.0.0.1:9/"}
```

```bash
ow --data-dir ~/.ow/controller dashboard --config dashboard.json --listen 127.0.0.1:8787
tailscale serve --bg http://127.0.0.1:8787   # then open https://NAME.TAILNET.ts.net/
```

- `hostname` is this machine's MagicDNS name; requests for any other Host are
  refused, and mutations, terminals and SSH need the exact `https://hostname`
  Origin, as with Access.
- `tailscale serve` replaces client-sent `Tailscale-User-*` and
  `X-Forwarded-For` headers: it sets the login only for a tailnet user (never for
  tagged devices) and `X-Forwarded-For` to the real peer address. Admission requires
  an `allowed_emails` match or the configured `allowed_capability` in the peer's
  whois `CapMap`. Whois must report the same login and an untagged device, even
  when a capability is present. The catalog subject is the stable Tailscale user ID from whois, so
  a login name reassigned to someone else does not inherit machines. Catalog
  identities use the issuer `tailscale://TAILNET.ts.net`.
- The dashboard runs `tailscale whois --json PEER` for each request, so the
  `tailscale` CLI must be on its `PATH`; tailscaled answers this read-only query
  for any local user.
- The CLI installer is not offered. Build `ow` from source (Linux or macOS:
  `cargo build --release -p ow`) and sign in with
  `ow login https://NAME.TAILNET.ts.net --tailnet`: no Cloudflare login, the
  request carries this device's tailnet user through `tailscale serve`, so
  `ow list`, `ow create`, `ow shell`, `ow ssh` and `ow ssh-config` work for an
  allowed login. The login checks that the dashboard admits this device.
- Anyone who can open a TCP connection to the loopback port on this machine can
  claim an allowed login. Use it only where the machine's local users are
  operators; guests cannot reach host loopback.
- Terminal, SSH and preview streams also recheck admission every 30 seconds and close on
  denial, a changed user ID or a failed lookup (up to five seconds per lookup).
  Their existing session deadlines still apply; reconnect to continue.
  Removing a login from `allowed_emails` takes effect on dashboard restart.
- `team_domain` and `audience` must be omitted; `upstream` is still required by
  the config format and unused by the dashboard.
- The AWS component can run this for you: see `dashboardLogins` and `dashboardCapability` in
  [deploy/aws](../deploy/aws/README.md).

### Admission through a capability grant

Choose a capability under a domain you control, and configure it once:

```json
{"auth": "tailscale", "hostname": "NAME.TAILNET.ts.net",
 "allowed_capability": "example.com/cap/open-workspaces", "upstream": "http://127.0.0.1:9/"}
```

A tailnet administrator can then define the group, authorize the host tag, assign
it to the dashboard host, and add a grant such as this to the existing policy:

```json
{"grants": [{
  "src": ["group:ow-users"], "dst": ["tag:ow-host"],
  "ip": ["tcp:443"],
  "app": {"example.com/cap/open-workspaces": [{}]}
}]}
```

The network permission permits HTTPS; the application permission admits the
user. See [Tailscale application capabilities](https://tailscale.com/docs/features/access-control/grants/grants-app-capabilities).
Only the top-level whois `CapMap` is used, not node attributes or a client-sent
`Tailscale-App-Capabilities` header. The capability is a Boolean admission grant;
its parameter objects do not grant catalog administration or other users' machines.

At least one of `allowed_emails` and `allowed_capability` is required. If both
are configured, either admits the user: removing their grant does **not** revoke
an email-allowlisted user. For grant-only administration, omit the email list.
The explicit email allowlist is still required for Cloudflare Access and for
`bootstrap_owner_email` legacy ownership migration.

After Tailscale propagates a policy removal to the host, the next request is
denied and existing terminal/SSH/preview streams close within 35 seconds. No dashboard
restart or per-person application config update is needed. Work already
dispatched to a guest is not cancelled by admission revocation; ownership and
machine data are preserved. The existing AWS in-place config path already avoids
host replacement; capability grants simplify administration rather than fixing
host replacement downtime.

Policy edits require the tailnet administrator's authorization. Before wider
admission, verify on the actual host that a granted user can access only their
machines, an ungranted user and tagged device are denied, a forged login/header
is denied, and grant removal denies the next request and closes live streams.
Local fake-daemon tests do not establish live tailnet propagation.

## Run and deployment

Build with `bash scripts/build-local.sh`, then ensure the local worker is running
with `./ow up`. The project ingress helper now starts the dashboard (using the
explicit prototype data directory) and the existing dedicated tunnel:

```bash
python3 scripts/remote-access.py status
python3 scripts/remote-access.py start
python3 scripts/remote-access.py stop
```

No DNS route, hostname, app audience or user policy was changed to switch the
counter page to the dashboard. Existing VM state was retained. The tunnel and
HTTP process are detached and have no automatic service installation or reboot
recovery; the machine must remain online. The old `./ow publish` relay can still
be used locally but is not the public dashboard.

## Observed verification

- Every dashboard feature and the browser test that covers it are listed in the
  [UI feature matrix](UI_FEATURES.md). That suite runs in CI against the real
  `ow dashboard` with a fake worker; a feature marked working whose test fails
  or disappears fails the build.
- Native JWT tests deny unsigned static/API routes, cross-origin mutations and
  arbitrary worker commands; verify CSP on authenticated HTML responses.
- Real browser test against a fresh dedicated worker passes create/exec, a paired
  memory/disk checkpoint, independent forks, hibernate/resume and checkpoint
  restore. An HTTP counter stored solely in guest process memory proves preserved
  state through those actions; guest files prove independent disk writes.
- Browser tests also prove guest PTY detection, shell state, Ctrl-C, Ctrl-Z/fg,
  resize, interactive vi editing, bounded heavy-output interruption, exit 42,
  disconnect cleanup and concurrent management exec.
- Browser test also checks escaped malicious HTML output, nonzero guest exit
  status, mobile horizontal overflow, resource view and JavaScript errors.
- Test-only synthetic RSA keys/auth assertions are generated in memory and bound
  to a local test router. There is no production authentication bypass. The test
  runs a local TLS terminator and real secure WebSocket with the actual browser
  HTTPS Origin. It does not establish that an actual Cloudflare browser session works.
- Public deployment checks verify anonymous requests to HTML, JavaScript and both
  API routes redirect to the configured Access login, and forged origin tokens
  return 401. Owner browser verification of the new UI remains a user check.

To reproduce (Linux/KVM/OpenSSL/Chromium required):

```bash
bash scripts/build-local.sh
npm install --prefix data/dashboard-tests --no-audit --no-fund playwright
export RUSTUP_HOME="$PWD/data/runtime-spike/rustup"
export CARGO_HOME="$PWD/data/runtime-spike/cargo"
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked dashboard_browser_real_vm -- --ignored --nocapture
```

The browser test closes its own worker, retains ignored screenshots/results and
never changes the owner's live machines. It uses the installed Chromium binary;
Playwright and its package metadata stay in ignored test storage.

Remote operations now pass through the SQLite ownership catalog; see
[per-user names, migration, identity and metadata](USERS.md). Existing remote
users no longer receive global machine/snapshot lists or other users' usage.

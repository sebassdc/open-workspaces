# Browser dashboard

The owner requested a dashboard on the **existing project hostname**, replacing
its guest counter demo. No additional hostname or domain was created. A Rust
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
  editors. The backend is Rust; the browser uses locally vendored xterm.js.
- Desktop and mobile layouts, operation progress/errors and polling updates.
  Restore and stop explain memory/disk consequences before the user's action.

The command console invokes `/bin/sh -c` for each command, with output after
completion. **Open Terminal** creates a separate interactive PTY inside the
selected running VM. See [terminal transport and limits](TERMINALS.md).
Guest networking now supports filtered IPv4 Internet egress. The page includes a copyable curl installer for the remote Linux x86-64 CLI; see [networking and CLI](NETWORKING.md). The API does not delete machines
or snapshots, change host configuration, grant Cloudflare permissions, or serve
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
  tagged devices) and `X-Forwarded-For` to the real peer address. The login must
  be in `allowed_emails`, and `tailscale whois` for that peer must report the
  same login. The catalog subject is the stable Tailscale user ID from whois, so
  a login name reassigned to someone else does not inherit machines. Catalog
  identities use the issuer `tailscale://TAILNET.ts.net`.
- The dashboard runs `tailscale whois --json PEER` (cached for a minute), so the
  `tailscale` CLI must be on its `PATH`; tailscaled answers this read-only query
  for any local user.
- The CLI installer and `ow login` are not offered: the remote CLI signs in
  through Cloudflare Access only.
- Anyone who can open a TCP connection to the loopback port on this machine can
  claim an allowed login. Use it only where the machine's local users are
  operators; guests cannot reach host loopback.
- Terminal and SSH streams close after eight hours; reconnect to continue.
  Removing a login from `allowed_emails` takes effect on restart.
- `team_domain` and `audience` must be omitted; `upstream` is still required by
  the config format and unused by the dashboard.
- The AWS component can run this for you: see `dashboardLogins` in
  [deploy/aws](../deploy/aws/README.md).

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

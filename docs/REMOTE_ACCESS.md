# Authenticated Internet access

## Current implementation

The deployed project hostname now serves the [browser dashboard](DASHBOARD.md),
replacing the counter demo at that same URL. Its authenticated handler manages
the local worker directly; no additional domain or hostname was created. The
standalone guest-app gateway described below remains available as `ow serve`,
but arbitrary guest HTML is not served on the management origin.

The local prototype now includes `ow serve`: a Rust gateway for **one guest HTTP
app**, protected by Cloudflare Access JWT verification. It binds only to loopback
and has no worker connection, workspace lifecycle API, terminal, or dashboard.
The existing `ow publish` loopback relay remains available for local use.

The owner authorized a dedicated Cloudflare setup for this project. Tunnel login,
named-tunnel creation, Access app/policy creation and public DNS routing succeeded.
The owner supplied an API token privately after the original tunnel credential
could not create an Access application. The dedicated tunnel and local gateway
are running. Browser-shaped anonymous and forged-token requests redirect to the
configured Access login; origin requests without valid assertions return 401.
Positive owner login, session expiry/logout and another-user denial still require
browser verification. A public login redirect alone does not prove app access.
Existing Hostinger apps, OpenClaw ingress, Tailscale listeners and global SSL
settings must remain untouched.

```text
Browser -> Cloudflare HTTPS/Access -> dedicated outbound tunnel
        -> ow serve (loopback; verify JWT) -> ow publish -> guest HTTP port
```

### Origin enforcement

- Require one RS256 assertion with a valid signature from the configured team's
  HTTPS signing-key endpoint. Verify issuer, application audience, expiry,
  not-before, nonempty subject and explicit email allowlist. Reject missing,
  malformed, duplicate, forged or mismatched assertions before guest forwarding.
- Fetch signing keys before listening; refresh every ten minutes. Cached keys
  expire after one hour and requests fail closed if refresh cannot recover.
  An unknown key ID is denied until a successful refresh.
- Accept only the configured public Host header. The upstream is a fixed
  `http://127.0.0.1:PORT` route, never a request-supplied address. Do not follow
  upstream or key-endpoint redirects. Ignore host proxy environment settings.
- Remove Cloudflare headers, Access cookies, Authorization, forwarded identity
  headers and hop-by-hop headers before requests reach the guest. Suppress guest
  responses attempting to set Access cookies. Guest application cookies remain.
- Require the matching HTTPS Origin header for POST/PUT/PATCH/DELETE and other
  unsafe methods. Guest apps must still implement their own application security,
  including avoiding mutations on GET. API clients must supply the configured
  Origin together with a valid user Access assertion; service tokens are not
  supported by this owner-email gateway.
- Buffer bodies with limits: requests 1 MiB, responses 8 MiB. At most 64 active
  forwarded requests, 45-second total handler timeout, 30-second upstream timeout.
  Disable response caching. WebSockets, SSE, raw TCP and streaming uploads are
  not supported in this initial slice. These bounds are not complete connection
  or host resource isolation.

Same-user host processes can access existing loopback relays directly. This gate
protects the Internet ingress; it does not isolate mutually untrusted host users.
The Firecracker jailer/cgroup and broader multi-user hardening gates remain open.
Never put a management dashboard on the guest application's origin. Keep dashboard
cookies scoped to its host; use a separate application domain when offering
untrusted guest applications to others.

## Setup workflow

1. Run `bash scripts/cloudflare-login.sh`. It installs only a project-local
   Linux x86_64 cloudflared 2026.9.3 binary, verifies the release asset SHA-256,
   and starts Cloudflare's browser login. The certificate is stored outside the
   repository in `~/.cloudflared`; never paste it into chat or commit it. Login
   authorizes tunnel administration; it does not establish a deployed Access
   application or a verified public route.
2. Select a dedicated hostname in a Cloudflare-managed zone. In Cloudflare Zero
   Trust, configure a self-hosted Access application for the exact hostname with
   an Allow policy for the owner's email. Do not add a Bypass policy. Record the
   team domain and application AUD tag privately. Access administration may
   require an additional scoped API token or authenticated dashboard session;
   a tunnel login certificate is not assumed to grant those permissions.

   The helper `python3 scripts/cloudflare-access.py authorize` accepts a token
   through a hidden local terminal prompt and stores it privately in ignored
   project data. Create a custom token scoped to the relevant account with:
   **Access: Apps and Policies — Edit**, and
   **Access: Organizations, Identity Providers, and Groups — Read**. The latter
   covers team-domain discovery and checking the existing One-time PIN provider.
   No global API key or additional DNS permission is needed for this token.

   Finish Zero Trust onboarding if needed. The helper selects an existing login
   provider and refuses to change global identity providers. After local
   publication, configure the dedicated app using example values:

   ```bash
   python3 scripts/cloudflare-access.py configure \
     --hostname app.example.com --email owner@example.com \
     --upstream http://127.0.0.1:12345
   ```

   This creates and verifies an email allow policy and prepares private origin
   and tunnel configurations, without publishing DNS. It refuses to adopt an
   unrelated Access app. To add users later, repeat `--email` with the complete
   desired list, including current users, and restart the gateway so both layers
   enforce the same list. Browser invitations and a user-management UI remain
   future work.

   The deployed owner-only pilot uses the existing Cloudflare login provider,
   selected with `--login-method cloudflare`; it also requires the owner's email
   in the app policy and gateway allowlist. The default helper selection is
   `--login-method onetimepin`. Email PIN was not added globally because two
   existing apps accept all configured providers. Existing apps/providers were
   left unchanged. Before admitting users who are not Cloudflare account members,
   enable a suitable app login method while preserving existing app authentication;
   adding an email alone does not grant it a new login method. Do not give workspace
   users Cloudflare account administration merely to let them use a workspace.
3. Publish the intended guest HTTP port locally with `./ow publish <workspace>`.
   Its URL becomes the fixed gateway upstream. Store the following configuration
   under ignored `data/remote-access/`, with mode 600. Values below are examples,
   not actual infrastructure identifiers:

   ```json
   {
     "hostname": "app.example.com",
     "team_domain": "example-team.cloudflareaccess.com",
     "audience": "ACCESS_APPLICATION_AUD",
     "allowed_emails": ["owner@example.com"],
     "upstream": "http://127.0.0.1:LOCAL_PUBLISHED_PORT"
   }
   ```

   Replace the placeholder port with the actual number. Run:

   ```bash
   chmod 600 data/remote-access/gateway.json
   ./ow serve --config data/remote-access/gateway.json --listen 127.0.0.1:8787
   ```

4. Create a dedicated named tunnel with private credentials and a dedicated
   configuration file in ignored storage. Map only the selected hostname to
   `http://127.0.0.1:8787`, with a final `http_status:404` catch-all. Enable Access
   token validation in cloudflared as well. Run that configuration explicitly;
   do not adopt an unrelated default tunnel config or change shared listeners.
   Create the DNS route only after the Access policy and origin gate are ready.
5. Verify from an external browser: unsigned traffic cannot reach the guest,
   the owner can sign in and use the app, another email is denied, direct origin
   calls without the assertion fail, logout/expiry deny subsequent requests,
   and tunnel/gateway shutdown removes access. Local tests are not proof that
   Cloudflare policy, DNS or login works.

If the browser login times out, rerun the login script for a fresh link. All
credentials, account/zone IDs, AUD tags and live hostnames stay out of commits.
No router forwarding or root privileges are needed for this tunnel design.

### Local ingress lifecycle

The tunnel and dashboard gateway are detached local processes; they reconnect while
running but are not installed as reboot-persistent services. Use:

```bash
python3 scripts/remote-access.py status
python3 scripts/remote-access.py stop
python3 scripts/remote-access.py start
```

Stop affects only the project tunnel/gateway, retaining the workspace worker,
guest disks and snapshots. PID ownership, command and start-time checks prevent
signalling an unrelated process from a stale descriptor. Start requires prepared
private configs and a running workspace worker. The dashboard command uses the
explicit prototype data directory and does not depend on the guest publish relay.
The host must remain online; automatic wake and
reboot recovery are separate unfinished capabilities.

## Validation and next slices

`cargo test --locked` exercises synthetic, ephemeral RSA-signed tokens and a real
loopback HTTP upstream: signature/issuer/audience/email/time/subject rejection,
algorithm confusion, duplicate assertions, Host mismatch, cross-origin mutation,
body bounds, denial before forwarding, credential stripping, response cookie
filtering and expired key-cache rejection. RSA test keys are generated with
OpenSSL in memory and discarded; no production authentication bypass exists.

Public deployment acceptance requires the external checks above. The browser
machine dashboard and authenticated lifecycle operations are now implemented;
see [its real-VM/browser tests](DASHBOARD.md). Native guest PTY WebSocket terminals are implemented and tested locally through
a TLS terminator; see [terminal details](TERMINALS.md). Remote CLI authentication,
workspace-specific authorization, wake-on-request and idle hibernation automation
remain subsequent slices.

`python3 experiments/runtime-spike/access-setup-test.py` additionally checks the
configuration helper offline: unrelated host/app changes are refused before
mutation, a missing PIN provider leaves global configuration untouched, and
adding users updates only the owned policy while retaining private configs and
the tunnel catch-all. These mocks do not establish Cloudflare write permission
or public login success.

References:
- [Cloudflare Tunnel downloads](https://developers.cloudflare.com/tunnel/downloads/)
- [Locally managed tunnel setup](https://developers.cloudflare.com/tunnel/features/locally-managed-tunnels/create-local-tunnel/)
- [Access application and origin validation](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/self-hosted-public-app/)
- [JWT verification and signing keys](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/)

## Public CLI installer

Only three fixed `/cli/` installer artifacts now bypass Access; dashboard/API/terminal requests still require owner login. See [exact routing, installer and remote CLI](NETWORKING.md).

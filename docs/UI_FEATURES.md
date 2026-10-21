# Dashboard feature matrix

Every browser dashboard feature and action, its status, the release it last
passed in, and the browser test that covers it. CI keeps this table honest: a
row marked **works** must name a test, and every test a row names must pass,
or the `dashboard-e2e` workflow fails. Removing, skipping or breaking the test
of a working feature therefore fails the build, and so does a pull request that
removes a **works** row or changes its status, unless it carries the
`ui-matrix-downgrade` label.

- **works**: the test exercises the feature end to end and passes.
- **partial**: only part of the feature works or is tested end to end; the note says which.
- **missing**: the dashboard does not offer it yet (or the code exists but does nothing).

**Last passed** is the release version whose dashboard code the test passed
against. Update it when the feature's code changes in a new release.

## How the tests run

`e2e/` holds a pinned Playwright suite. It starts the real `ow dashboard`
binary with tailnet sign-in, behind a local TLS proxy that plays
`tailscale serve`, and a fake runner worker on the dashboard's `control.sock`
that keeps machines and snapshots in memory and runs an echo shell for the
terminal. The dashboard's API validation, ownership catalog, quotas and
operation journal are the real ones; no guest VM runs. A test can give one of its
machines a committed placement, as a move between runners leaves it, through the
harness-only `/__e2e/manage` route of that proxy; the fake worker then refuses any
other placement for it, as the real worker does. Each test signs in as
its own tailnet user, so tests never see each other's machines. The real-VM
browser test in `crates/ow/src/remote.rs` (`dashboard_browser_real_vm`) remains
the check that actual guests work.

```bash
cargo build --locked -p ow          # Linux: the dashboard does not run on macOS
cd e2e && npm ci && npx playwright install --with-deps chromium
npx playwright test                  # writes playwright-report/results.json
node check-matrix.mjs                # this table against that run
```

To add a feature: write its test in `e2e/tests/`, then add a row here naming it
as `` `file › title` `` (with any `describe` title in between). Several tests
go in one cell separated by `;`. A test no row names is reported as a note.

## Machines

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Machine list: cards, counts, selection | works | v0.9.22 | `machines.spec.mjs › machine list shows cards, counts and the selected machine` |
| Card content top-aligned; page notice spaced from the card below | works | v0.9.27 | `machines.spec.mjs › cards keep their content at the top, whatever their height` |
| Phone width: tapping a card scrolls its actions into view | works | v0.9.27 | `machines.spec.mjs › on a phone, tapping a card brings its actions into view` |
| Empty sandbox and empty snapshot list | works | v0.9.22 | `machines.spec.mjs › empty sandbox invites a first machine` |
| Memory counter: your usage against your owner limits | works | v0.9.23 | `machines.spec.mjs › memory counter shows your usage against your owner limits` |
| Create dialog: name, image, memory, vCPU, disk | works | v0.9.22 | `machines.spec.mjs › create dialog creates a machine with the chosen shape` |
| Refused create shown in the dialog | works | v0.9.22 | `machines.spec.mjs › create dialog shows a refused create and stays open` |
| Create dialog marks images the pool cannot run (disabled, "not available on this pool") | works | v0.9.29 | `machines.spec.mjs › create dialog marks images the pool cannot run` |
| Create dialog shows each node's free vCPU and memory, disables a full node, and explains refusals in dashboard words | works | v0.9.29 | `machines.spec.mjs › create dialog shows free room per node, disables a full one and explains a refusal plainly` |
| Create dialog offers only memory and vCPU sizes the chosen node (or any node, when automatic) can place | works | v0.9.29 | `machines.spec.mjs › create dialog offers only sizes the chosen node can place` |
| Create dialog offers only images the chosen node has ready | works | v0.9.29 | `machines.spec.mjs › create dialog offers only images the chosen node has ready` |
| Create dialog disables all sizes and blocks submission when the pool is full | works | v0.9.29 | `machines.spec.mjs › create dialog blocks a full pool without submitting a shape` |
| Create dialog blocks a node with no eligible images, clears the unavailable selections, and recovers when a ready node is chosen | works | v0.9.29 | `machines.spec.mjs › create dialog blocks a node without images and recovers on a ready node` |
| Create dialog on an ARM-only pool starts on a Mac runner shape (512 MiB or more, no disk size) | works | v0.9.29 | `machines.spec.mjs › create dialog starts an ARM-only pool on a shape its Mac runner accepts` |
| Create with a name already in use says the name is taken and stays open | works | v0.9.29 | `machines.spec.mjs › create dialog says a name is taken and stays open` |
| Create dialog starts with focus on the machine name | works | v0.9.29 | `machines.spec.mjs › create dialog starts on the machine name` |
| Rename in place, selection and console history follow | works | v0.9.22 | `machines.spec.mjs › rename keeps the machine selected under its new name` |
| Recovery status: neutral while the first point is pending, warning only past the one-hour target or on an upload error, relative time | works | v0.9.26 | `states.spec.mjs › a new machine shows a neutral first recovery point`; `states.spec.mjs › recovery status warns only past the one-hour target and shows relative time` |
| Navigation and resource usage view | works | v0.9.22 | `machines.spec.mjs › navigation switches between machines, snapshots and usage` |
| Signed-in account shown in the top bar | works | v0.9.30 | `machines.spec.mjs › top bar shows the signed-in account`; `states.spec.mjs › narrow width › a long signed-in email does not widen the phone layout` |

## Lifecycle

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Hibernate and resume | works | v0.9.22 | `lifecycle.spec.mjs › hibernate and resume keep the machine` |
| Stop with its warning, then start | works | v0.9.22 | `lifecycle.spec.mjs › stop explains what is lost, then start brings it back` |
| Resize a stopped machine's disk | works | v0.9.22 | `lifecycle.spec.mjs › resize grows a stopped machine disk` |
| Delete with snapshot warnings | works | v0.9.22 | `lifecycle.spec.mjs › delete warns about snapshots and removes the machine`; `lifecycle.spec.mjs › delete of a machine without snapshots says so` |
| Resize dialog prefilled with the current size; the same size answers "Already N GiB" | works | v0.9.27 | `lifecycle.spec.mjs › resize grows a stopped machine disk` |
| Destructive confirms (delete, restore) lead with a warning and use a danger button | works | v0.9.27 | `lifecycle.spec.mjs › delete warns about snapshots and removes the machine`; `snapshots.spec.mjs › fork and restore from the snapshot list` |
| Moved machine (placed by a move between runners): snapshot, stop, resize the disk and delete with its snapshots | works | v0.9.39 | `managed.spec.mjs › a moved machine snapshots, stops, resizes and deletes with its snapshots` |
| Moved machine: resize memory and vCPUs while stopped | works | v0.9.39 | `managed.spec.mjs › a moved machine resizes its memory and vCPUs while stopped` |

## Terminal and console

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Interactive terminal: open, echo, shell exit, disconnect | works | v0.9.31 | `console.spec.mjs › terminal opens, echoes input and reports the shell exit` |
| Terminal after the shell exits: Close and New session | works | v0.9.31 | `console.spec.mjs › terminal opens, echoes input and reports the shell exit` |
| Terminal dark scrollbar (dark color scheme) | works | v0.9.31 | `console.spec.mjs › terminal opens, echoes input and reports the shell exit` |
| Terminal notices a machine hibernated or stopped elsewhere and blocks input | works | v0.9.31 | `console.spec.mjs › terminal notices a machine hibernated elsewhere and blocks input`; `console.spec.mjs › terminal explains a machine stopped elsewhere after its connection drops` |
| Command console: output as text, exit status, clear | works | v0.9.22 | `console.spec.mjs › command console shows output and exit status as text` |
| Command console: 30 s limit stops the command with a plain message; the next command runs immediately with only its own output | works | v0.9.32 | `console.spec.mjs › a command past the 30 s limit shows a plain message, not the operation journal` |

## Snapshots and forks

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Snapshot dialog and snapshot list | works | v0.9.22 | `snapshots.spec.mjs › snapshot dialog saves a named checkpoint` |
| Fork a running machine (new capture) | works | v0.9.22 | `snapshots.spec.mjs › fork from a running machine creates an independent child` |
| Live fork's own capture labelled with the fork it was taken for | works | v0.9.26 | `snapshots.spec.mjs › fork from a running machine creates an independent child` |
| Failed capture names its machine and when it failed | works | v0.9.26 | `snapshots.spec.mjs › a failed capture names its machine and when it failed` |
| Fork and restore from the snapshot list | works | v0.9.22 | `snapshots.spec.mjs › fork and restore from the snapshot list` |
| Fork refused for room names the parent runner and the fork's shape, without CLI hints | works | v0.9.28 | `snapshots.spec.mjs › fork refused for room on the parent runner says so plainly` |

## Sharing and previews

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Share dialog, shared-with-me view, unshare | works | v0.9.22 | `sharing.spec.mjs › share dialog grants a teammate, who sees it under shared with me` |
| Share dialog rejects an invalid email | works | v0.9.22 | `sharing.spec.mjs › share dialog refuses an invalid email` |
| Share dialog refuses your own login in plain words | works | v0.9.28 | `sharing.spec.mjs › share dialog refuses your own login plainly` |
| Transfer dialog gives a machine to another owner; both see it under Ownership changes | works | v0.9.36 | `admin.spec.mjs › transfer dialog gives a machine to another owner, and both see the change` |
| Pool admin creates a machine for someone; the owner forks it for a teammate (owner field in create and fork dialogs) | works | v0.9.36 | `admin.spec.mjs › pool admin creates a machine for someone, who forks it for a teammate` |
| Pool admin sees other owners' machines under Pool machines and transfers one (fork and transfer only) | works | v0.9.36 | `admin.spec.mjs › pool admin sees and transfers another owner's unshared machine` |
| Publish and unpublish a port | partial | v0.9.22 | `previews.spec.mjs › publish port dialog publishes, links and unpublishes a preview`; `previews.spec.mjs › publish port is hidden when previews are not configured` |

Previews need `tailscale serve` and a root-owned helper on the host, so the
publish test answers only the publish/unpublish calls and the `previews` flag
itself (hence partial); the machine list and every other call reach the real
dashboard.

## Per-box AWS access

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| AWS role dialog: attach a role from the operator allow-list, show it with its token expiry, detach | works | v0.9.37 | `aws.spec.mjs › aws role dialog attaches an allowed role, shows its token and detaches it` |
| AWS role refused off the allow-list, not inherited by a fork, not offered to a use grantee | works | v0.9.37 | `aws.spec.mjs › aws role is refused off the allow-list, not inherited by a fork, and not offered to a use grantee` |
| Pool administrator attaches/detaches allowed AWS roles without a share; ordinary users cannot | works | v0.9.37 | `aws.spec.mjs › pool administrator manages AWS roles without a share and ordinary users cannot` |
| Configured pool admin manages unshared AWS roles through Pool machines without guest controls | works | v0.9.37 | `aws.spec.mjs › configured pool admin manages an unshared box role without guest access` |

The fake worker accepts the token delivery call; that a guest's AWS SDK turns
the token into credentials is checked by `crates/ow-core/tests/web_identity_chain.rs`
against a mock STS (see [per-box AWS access](AWS_ACCESS.md)).

## Host pool

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Invite a Linux host (one-use invitation, cleared on close) | works | v0.9.22 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` |
| Adjust an enrolled host's budget | partial | v0.9.22 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` (refusal for a host that never enrolled; the success path needs an enrolled runner) |
| Revoke a host | partial | v0.9.22 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` (revokes an invited host that never enrolled) |
| Pool banner says who can see guest data; invitation policy only in the invite dialog | works | v0.9.23 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` |
| Host controls only for the pool owner | works | v0.9.22 | `hosts.spec.mjs › host controls are hidden from other users` |

## Errors, sign-in and layout

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Worker outage error, recovery on refresh | works | v0.9.22 | `states.spec.mjs › worker outage shows an error and recovers on refresh` |
| Failed first load: retry in the grid, New machine disabled | works | v0.9.23 | `states.spec.mjs › failed first load offers a retry and keeps New machine disabled` |
| Expired login message | works | v0.9.33 | `states.spec.mjs › expired login asks the user to reload and sign in`; `dialogs.spec.mjs › HTTP 401 asks to sign in even with a JSON response`; `dialogs.spec.mjs › HTTP 403 asks to sign in even with a JSON response` |
| CLI installer panel (hidden on tailnet sign-in) | partial | v0.9.22 | `states.spec.mjs › the tailnet dashboard offers no CLI installer` (the Access sign-in panel and its copy button are not covered) |
| Sidebar CLI download link (hidden on tailnet sign-in) | partial | v0.9.30 | `states.spec.mjs › the tailnet dashboard hides the sidebar CLI download` (the link shown where the binary is served is not covered) |
| Phone width: no horizontal overflow, dialogs fit | works | v0.9.22 | `states.spec.mjs › narrow width › phone layout has no horizontal overflow and dialogs fit` |
| Keyboard focus kept on the same card or button across the 5 s refresh | works | v0.9.25 | `focus.spec.mjs › focus stays on an action button and a machine card across a refresh`; `focus.spec.mjs › focus stays on a machine card when its disk usage changes`; `focus.spec.mjs › focus stays on the same port Unpublish when an earlier port goes away` |
| Gateway error (502/504) says the server is not answering, clears on recovery | works | v0.9.33 | `dialogs.spec.mjs › a gateway error says the server is not answering, not that the login expired` |
| No console errors on load and dialog open; name patterns compile | works | v0.9.33 | `dialogs.spec.mjs › no console errors on page load and dialog open` |

## Dialogs and notices

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Invalid name refused in the browser, allowed characters named | works | v0.9.33 | `dialogs.spec.mjs › an invalid name is refused in the browser and names the allowed characters` |
| Dialog shows its result and unlocks before the state reload answers | works | v0.9.33 | `dialogs.spec.mjs › a dialog unlocks with its result while the state reload is slow` |
| Retry clears the previous error; busy label while it runs | works | v0.9.33 | `dialogs.spec.mjs › the snapshot dialog clears the last error and shows its progress` |
| Cancelled dialog leaves no stale notice | works | v0.9.33 | `dialogs.spec.mjs › a refused operation leaves no notice behind once its dialog is cancelled` |
| Result notice cleared on view or machine switch | works | v0.9.33 | `dialogs.spec.mjs › a result notice is cleared when the user switches view or machine` |
| Command result not shown after leaving its view or machine mid-run | works | v0.9.33 | `dialogs.spec.mjs › a command result started elsewhere does not follow the user to another view or machine` |
| Gateway error on an operation asks to try again | works | v0.9.33 | `dialogs.spec.mjs › a gateway error on an operation asks to try again, since an open dialog does not poll` |
| Unexpected HTML without an auth redirect does not claim the login expired | works | v0.9.33 | `dialogs.spec.mjs › an unexpected HTML response without an auth redirect does not ask to sign in` |
| Host node label rejects invalid characters and names the 64-character limit | works | v0.9.33 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` |

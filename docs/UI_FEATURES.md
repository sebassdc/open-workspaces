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
operation journal are the real ones; no guest VM runs. Each test signs in as
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
| Card content top-aligned; page notice spaced from the card below | works | v0.9.22 | `machines.spec.mjs › cards keep their content at the top, whatever their height` |
| Phone width: tapping a card scrolls its actions into view | works | v0.9.22 | `machines.spec.mjs › on a phone, tapping a card brings its actions into view` |
| Empty sandbox and empty snapshot list | works | v0.9.22 | `machines.spec.mjs › empty sandbox invites a first machine` |
| Create dialog: name, image, memory, vCPU, disk | works | v0.9.22 | `machines.spec.mjs › create dialog creates a machine with the chosen shape` |
| Refused create shown in the dialog | works | v0.9.22 | `machines.spec.mjs › create dialog shows a refused create and stays open` |
| Rename in place, selection and console history follow | works | v0.9.22 | `machines.spec.mjs › rename keeps the machine selected under its new name` |
| Navigation and resource usage view | works | v0.9.22 | `machines.spec.mjs › navigation switches between machines, snapshots and usage` |
| Signed-in account shown in the top bar | missing | — | The page reads `user` from `/api/state`, which the API does not return, so it always shows "Your account"; the login is available at `discovery.identity.email`. |

## Lifecycle

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Hibernate and resume | works | v0.9.22 | `lifecycle.spec.mjs › hibernate and resume keep the machine` |
| Stop with its warning, then start | works | v0.9.22 | `lifecycle.spec.mjs › stop explains what is lost, then start brings it back` |
| Resize a stopped machine's disk | works | v0.9.22 | `lifecycle.spec.mjs › resize grows a stopped machine disk` |
| Delete with snapshot warnings | works | v0.9.22 | `lifecycle.spec.mjs › delete warns about snapshots and removes the machine`; `lifecycle.spec.mjs › delete of a machine without snapshots says so` |
| Resize dialog prefilled with the current size; the same size answers "Already N GiB" | works | v0.9.22 | `lifecycle.spec.mjs › resize grows a stopped machine disk` |
| Destructive confirms (delete, restore) lead with a warning and use a danger button | works | v0.9.22 | `lifecycle.spec.mjs › delete warns about snapshots and removes the machine`; `snapshots.spec.mjs › fork and restore from the snapshot list` |

## Terminal and console

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Interactive terminal: open, echo, shell exit, disconnect | works | v0.9.22 | `console.spec.mjs › terminal opens, echoes input and reports the shell exit` |
| Command console: output as text, exit status, clear | works | v0.9.22 | `console.spec.mjs › command console shows output and exit status as text` |

## Snapshots and forks

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Snapshot dialog and snapshot list | works | v0.9.22 | `snapshots.spec.mjs › snapshot dialog saves a named checkpoint` |
| Fork a running machine (new capture) | works | v0.9.22 | `snapshots.spec.mjs › fork from a running machine creates an independent child` |
| Fork and restore from the snapshot list | works | v0.9.22 | `snapshots.spec.mjs › fork and restore from the snapshot list` |

## Sharing and previews

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Share dialog, shared-with-me view, unshare | works | v0.9.22 | `sharing.spec.mjs › share dialog grants a teammate, who sees it under shared with me` |
| Share dialog rejects an invalid email | works | v0.9.22 | `sharing.spec.mjs › share dialog refuses an invalid email` |
| Publish and unpublish a port | partial | v0.9.22 | `previews.spec.mjs › publish port dialog publishes, links and unpublishes a preview`; `previews.spec.mjs › publish port is hidden when previews are not configured` |

Previews need `tailscale serve` and a root-owned helper on the host, so the
publish test answers only the publish/unpublish calls and the `previews` flag
itself (hence partial); the machine list and every other call reach the real
dashboard.

## Host pool

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Invite a Linux host (one-use invitation, cleared on close) | works | v0.9.22 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` |
| Adjust an enrolled host's budget | partial | v0.9.22 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` (refusal for a host that never enrolled; the success path needs an enrolled runner) |
| Revoke a host | partial | v0.9.22 | `hosts.spec.mjs › host pool owner invites, budgets and revokes a Linux host` (revokes an invited host that never enrolled) |
| Host controls only for the pool owner | works | v0.9.22 | `hosts.spec.mjs › host controls are hidden from other users` |

## Errors, sign-in and layout

| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Worker outage error, recovery on refresh | works | v0.9.22 | `states.spec.mjs › worker outage shows an error and recovers on refresh` |
| Expired login message | works | v0.9.22 | `states.spec.mjs › expired login asks the user to reload and sign in` |
| CLI installer panel (hidden on tailnet sign-in) | partial | v0.9.22 | `states.spec.mjs › the tailnet dashboard offers no CLI installer` (the Access sign-in panel and its copy button are not covered) |
| Phone width: no horizontal overflow, dialogs fit | works | v0.9.22 | `states.spec.mjs › narrow width › phone layout has no horizontal overflow and dialogs fit` |

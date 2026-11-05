# GitHub Actions runner pools

The controller can keep GitHub Actions runners ready for a repository: `warm`
single-use runners, each in its own [ephemeral](AGENT_CLI.md) box, registered
with GitHub and idle before any job is queued. GitHub hands a queued job to an
idle runner as it does to its own hosted runners, so a job starts in the time
GitHub takes to assign it. The pool replaces each runner that takes a job in
the background, and deletes a box as soon as GitHub drops its runner after the
job.

Nothing runs on a runner host: the pools are part of the controller, configured
in its settings store, and act through a [workload token](USERS.md#workload-tokens)
(so its owner's limits and its placement pool apply).

## Set up

1. A placement pool with one or more runners for CI boxes, and a workload token
   bound to it: `ow node-pool NODE ci`, `ow token issue ci-runners --owner
   ci@example.com --pool ci`. The token's value is not needed: the pools use it
   by name.
2. An image with the GitHub runner and the start command (see [Image](#image)),
   built on those runners.
3. A GitHub App installed on the repository, with **Administration: read and
   write** (self-hosted runners) on it. Store its key and id:

   ```sh
   ow actions app-key set --key-file app.private-key.pem   # "-" reads standard input
   ow config set actions.github_app_id 123456
   ```

4. The pools:

   ```sh
   ow config set actions.pools '[{"name":"ci","repo":"acme/app","labels":["self-hosted","Linux","X64","app-ci"],
     "token":"ci-runners","image":"ci-runner","memory_mib":4096,"vcpu_count":2,"warm":10,"max":10}]'
   ow actions status
   ```

Jobs reach the pool with `runs-on: [self-hosted, Linux, X64, app-ci]`. Every
label a job asks for must be one of the pool's, and the pool must have one beyond
`self-hosted`, `Linux` and `X64`, so other self-hosted jobs never land on it.

## Settings

| Setting | Meaning |
| --- | --- |
| `actions.pools` | A JSON list of pools; `[]` (the default) is none. Read on every 2 s pass. |
| `actions.github_app_id` | The GitHub App's id; `0` is none. |

Each pool:

| Field | Meaning |
| --- | --- |
| `name` | 1 to 20 lowercase letters, digits or `-`; it prefixes each box's name. |
| `repo` | `OWNER/NAME`: runners are registered on this repository. |
| `labels` | The runners' labels (1 to 16). |
| `token` | The workload token the pool's boxes are made with (by name). |
| `image` | The box image. |
| `memory_mib`, `vcpu_count`, `disk_gib` | Box size; default 4096, 2 and the create default. |
| `warm` | Runners kept idle (starting or online). |
| `max` | Boxes at most, busy included. `warm <= max <= 64`. |

**Changes apply live.** A new image, size, labels or repository: idle boxes of
the old shape are replaced, and busy ones finish their job first. A lower `warm`
or `max` makes no new boxes until the pool is under it; a removed pool's idle
boxes are deleted and its busy ones finish. `[]` turns every pool off.

## How a box is made and removed

Each box has its own thread and its own catalog connection, so creates,
hand-offs and deletes run side by side and do not wait on the dashboard.

1. **Create** the box through the pool's token (ephemeral, in the token's pool).
2. **Register**: a single-use runner registration (JIT config) from the App.
3. **Hand off**: put the registration at `/home/dev/.ow-ci/jitconfig` and run
   `ow-ci-runner-start` in the box.
4. **Idle** until GitHub assigns a job; **busy** while it runs.
5. **Remove** once GitHub drops the runner (or lists it offline after a job):
   unregister if needed, stop and delete the box.

A launch that fails (a full runner, an image missing) removes what it made and
holds that pool's new boxes until one of its boxes is removed, or for 10 s; other
pools carry on. An idle runner that GitHub just gave a job is never removed: a
removal for a settings change or age asks GitHub first, which refuses a busy
runner.

Limits: a runner not online 5 minutes after it started is removed, as is an idle
runner after 6 hours (replaced by a fresh one) and any box after 3 hours. A
controller restart removes the boxes it was still making or removing and keeps
the started ones.

**GitHub calls.** One list of the repository's runners per pass (`ETag`, so an
unchanged list costs no rate limit), one registration per box, and one
unregister only when an idle runner is removed. The App key lives in a 0600 file
in the controller's data directory and in memory; no key, token or registration
is logged, returned or put in the catalog.

## Image

The image must provide:

- the GitHub Actions runner, run by the `dev` user;
- `/home/dev/.ow-ci/` (owned by `dev`, mode 0700);
- `ow-ci-runner-start`: reads the registration from
  `/home/dev/.ow-ci/jitconfig`, deletes the file, and starts the runner in the
  background, exiting 0 within the exec time limit.

The runner takes one job and exits. A job-started hook
(`ACTIONS_RUNNER_HOOK_JOB_STARTED`) that refuses pull requests from forks is a
good second guard for a repository that might ever run them.

## Status

`ow actions status [--pool P]` ([actions.status](ADMIN_API.md#actionsstatus)):
each pool's boxes by phase, whether it is held, its last error, and its latest
boxes' start times (create sent, box ready, registered, started, online, busy,
removed).

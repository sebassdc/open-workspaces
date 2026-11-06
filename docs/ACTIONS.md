# GitHub Actions runner pools

The controller can keep GitHub Actions runners ready for a repository, or for
every repository of an organisation that a runner group allows: `warm`
single-use runners, each in its own [ephemeral](AGENT_CLI.md) box, registered
with GitHub and idle before any job is queued. GitHub hands a queued job to an
idle runner as it does to its own hosted runners, so a job starts in the time
GitHub takes to assign it. The pool replaces each runner that takes a job in
the background, and deletes a box as soon as GitHub drops its runner after the
job.

Nothing runs on a runner host: the pools are part of the controller, configured
in its settings store, and act through a [workload token](USERS.md#workload-tokens)
(so its owner's limits and its placement pool apply). To bound a pool by its
`max` and its runners alone, make the token's owner unlimited
(`ow limits set OWNER --unlimited`, [LIMITS.md](LIMITS.md#setting-an-owners-limits)).

## Set up

1. A placement pool with one or more runners for CI boxes, and a workload token
   bound to it: `ow node-pool NODE ci`, `ow token issue ci-runners --owner
   ci@example.com --pool ci`. The token's value is not needed: the pools use it
   by name.
2. An image with the GitHub runner and the start command (see [Image](#image)),
   built on those runners.
3. A GitHub App installed on the repository, with **Administration: read and
   write** (self-hosted runners) on it; for an [organisation pool](#organisation-pools),
   installed on the organisation with **Self-hosted runners: read and write**, and
   **Actions: read** on every repository in its `repos` (spill reads their queued jobs).
   Store its key and id:

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
| `name` | 1 to 16 lowercase letters, digits or `-`; it prefixes each box's name. |
| `repo` | `OWNER/NAME`: runners are registered on this repository. |
| `org` | `OWNER`, in place of `repo`: runners are registered on this organisation (see [Organisation pools](#organisation-pools)). |
| `runner_group_id` | The runner group they join; default 1, GitHub's default group. |
| `repos` | Organisation pools only: up to 8 `OWNER/NAME` of the organisation whose queued jobs may [spill](#spill); none: no spill. |
| `labels` | The runners' labels (1 to 16). |
| `token` | The workload token the pool's boxes are made with (by name). |
| `image` | The box image. |
| `memory_mib`, `vcpu_count`, `disk_gib` | Box size; default 4096, 2 and the create default. |
| `warm` | Runners kept idle (starting or online). |
| `max` | Boxes at most, busy included. `warm <= max <= 64`. |
| `aws_role` | Optional. A [per-box AWS role](AWS_ACCESS.md) (`arn:aws:iam::ACCOUNT:role/ow-box/NAME`) attached to each box right after it is made, before its runner registers, as `ow aws-role set` would for the token's owner: the role must be on the operator's allow-list for that owner. Jobs then reach AWS through the standard credential chain, with no key in the repository or the box. |

**Changes apply live.** A new image, size, labels, repository, organisation or runner group: idle boxes of
the old shape are replaced, and busy ones finish their job first. A lower `warm`
or `max` makes no new boxes until the pool is under it; a removed pool's idle
boxes are deleted and its busy ones finish. `[]` turns every pool off.

## Organisation pools

A pool with `org` in place of `repo` registers its runners on the organisation,
so one warm set serves every repository its runner group lets use them, instead
of one pool, with its own idle boxes, per repository. Each job still gets a
fresh single-use box, deleted after it.

```sh
ow config set actions.pools '[{"name":"ci","org":"acme","runner_group_id":3,"repos":["acme/app","acme/api"],
  "labels":["self-hosted","Linux","X64","acme-ci"],"token":"ci-runners","image":"ci-runner","warm":10,"max":12}]'
```

- **Which repositories.** GitHub decides from the runner group: the default
  group (1) lets every private repository of the organisation use the runners;
  a group of their own, limited to selected repositories, keeps the rest out.
  Repositories still reach the runners only by asking for the pool's labels.
- **Spill** asks about the queued jobs of the repositories in `repos` (each
  one adds its calls, see [Spill](#spill)); without `repos` the pool keeps
  its warm runners and never spills.
- **Fairness.** Repositories share the warm runners first come, first served.
  To guarantee each one a share instead, give each its own repository pool.
- **Usage by repository.** The controller sees boxes, not repositories: each
  job's runner name (`POOL-<12 hex>`) is in the repository's job list on
  GitHub, which gives the jobs and runner minutes per repository.

## How a box is made and removed

Each box has its own thread, so one box's GitHub calls and waits never hold up
another's. Their box operations go through the catalog like any client's: the
catalog is held only for its own bookkeeping, never during the runner's work, and a
runner boots new boxes side by side, so a burst of jobs larger than the warm pool
gets its boxes in parallel.

1. **Create** the box through the pool's token (ephemeral, in the token's pool).
   A full runner then costs no GitHub call.
   With `aws_role`, the role is attached now; a refusal fails the launch.
2. **Register**: a single-use runner registration (JIT config) from the App.
3. **Hand off**: put the registration at `/home/dev/.ow-ci/jitconfig` and run
   `ow-ci-runner-start` in the box.
4. **Idle** until GitHub assigns a job; **busy** while it runs.
5. **Remove** once GitHub drops the runner (or lists it offline after a job):
   unregister if needed, stop and delete the box.

A launch that fails (a full runner, an image missing) removes what it made and
holds that pool's new boxes until one of its boxes is removed, or for 10 s; other
pools carry on. While GitHub cannot be read for a repository (no App id or key,
the App not installed there, an outage, a rate limit) its pools make no box and
its boxes are removed only when they reach a limit; other repositories' pools
carry on.

**Drains.** Idle runners on a runner being drained are removed (busy ones finish
their job), and the drain deletes stopped ephemeral boxes instead of parking
them, so a drain of a CI runner finishes once its jobs do. An idle runner that GitHub just gave a job is never removed: a
removal for a settings change or age asks GitHub first, which refuses a busy
runner.

Limits: a runner not online 5 minutes after it started is removed; an idle runner
is replaced by a fresh one after 6 hours; a job still running after 3 hours is
cut off and its box removed. A controller restart removes the boxes it was still
making or removing and keeps the started ones. A pool's boxes are removed with
the workload token they were made with: revoke a token only after its pool's
boxes are gone.

**GitHub calls.** One list of the repository's (or organisation's) runners per pass (`ETag`, so an
unchanged list costs no rate limit), one registration per box, and one
unregister only when an idle runner is removed. With spill, the queued-job
lists in [Spill](#spill), only while the pool is saturated. The App key lives in a 0600 file
in the controller's data directory and in memory; no key, token or registration
is logged, returned or put in the catalog: journals keep a put's contents only as
their SHA-256.

## Spill

When the pool's placement pool has a [spill rule](MULTINODE.md#spill), a burst
larger than the pool's own runners can hold borrows room on the rule's runner,
without keeping anything warm there:

- **Warm runners stay home.** Boxes made to keep `warm` idle runners ask not to
  spill (`"spill": false`). When the pool's own runners are full, warm boxes wait
  until one of the pool's boxes is removed (or 60 s); a spilled idle box never
  counts as warm.
- **Queued jobs get boxes.** Only while every box of the pool is busy and it is
  under `max`, the controller asks GitHub, at most every 10 s, how many of the
  repository's queued jobs ask only for the pool's labels: one list of queued
  and one of running workflow runs (the newest 10 of each) and one job list per
  run, at most 22 calls per repository (an organisation pool: per entry of `repos`), and none while fewer than 1000 calls of the App's rate
  limit are left, so the pool's own calls always have room. Each queued job no
  idle or starting runner will take gets a box that may spill, within `max`.
- **Spilled boxes are single-use and short-lived.** Like every pool box, one is
  deleted when its job ends. One that is online with no job for 60 s (another
  runner took the job) is removed. Ephemeral boxes are never hibernated,
  snapshotted, moved or parked.
- `max` caps the pool's boxes everywhere; the spill rule's `max_boxes` caps how
  many of them a borrowed runner holds. Set `max` to what the pool's own runners
  hold plus what it may borrow: without a rule the extra boxes are never asked
  for.

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
each pool's boxes by phase (and how many spilled), whether it is held
(`held_s`, and `home_held_s` for warm boxes), its last error, and its latest
boxes' start times (create sent, box ready, registered, started, online, busy,
removed).

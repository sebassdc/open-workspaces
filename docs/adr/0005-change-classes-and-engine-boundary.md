# ADR 0005: change classes, split units and in-place host upgrades
Date: 2026-10-17 | Status: implemented in deploy/aws 0.5.0; sandbox EC2 check recorded in docs/UPGRADES.md.

Every change gets a class, and a host takes the least disruptive path its class
allows:

| Class | What restarts | Running boxes |
| --- | --- | --- |
| 0 config | nothing, or a file is rewritten | untouched |
| 1 control plane | controller and node agent, or the dashboard | untouched; open SSH and terminal streams reconnect |
| 2 runner | the worker | hibernated, swapped, woken; processes continue when every capture still matches |
| 3 host | the instance (AMI, subnet, stage 0) | hibernated, host replaced |

Decisions:

- The worker that owns the Firecracker processes runs in its own systemd unit
  (`ow supervisor`, hibernate on stop). The controller and node agent run as
  `ow host up --external-worker`, which never starts a worker. Units are tied by
  `Wants=`/`After=` only, so a control-plane restart never restarts the worker.
  Re-adopting running VMMs from a new worker is not done: VMMs die with their
  worker (parent-death signal, PTY master, stderr pipe, namespaces, uplink).
- User data carries stage 0 only. Everything else is one SSM String parameter
  (`/open-workspaces/NAME/config`), applied by `ow-host apply`, so a config or
  release change never replaces the instance.
- Releases are immutable directories with a pin per unit (`run/worker`,
  `run/agent`, `run/dashboard`). Each release has its own asset set, a reflink
  copy, so staging never writes a file a running process uses.
- Each release declares its impact in `release-impact.json`; `ow-host upgrade`
  plans from it, refuses what cannot be done in place, and runs a journaled
  state machine with health checks per pin, abort before the switch and
  rollback after it. CI checks the manifest against the code and the diff.
- The host-runner protocol has a window (`PROTOCOL_MIN..=PROTOCOL`), a refused
  heartbeat closes with its reason, and catalogs carry a reader floor
  (`settings.min_reader_schema`) so an older binary can roll back across
  additive migrations.
- Host orchestration is Python 3 standard library (`deploy/aws/host/`); engine
  changes stay in Rust. The release builds on the host for now; prebuilt
  binaries are a follow-up.

Not decided here: worker re-adoption of running VMMs, keeping the previous VMM
and guest kernel side by side so a runtime bump resumes warm, a worker
`configure` operation for budgets, and dashboard login reload without restart.

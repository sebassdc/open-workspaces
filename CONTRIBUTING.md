# Contributing

Read [README.md](README.md), [docs/HANDOFF.md](docs/HANDOFF.md) and the
engineering instructions in [AGENTS.md](AGENTS.md) before changing code. They
apply to every contribution, human or agent.

## Configuration: the controller is the single source

The controller host is the single source of configuration and state. Runners
are dumb workers that receive their configuration from the controller; nothing
is configured separately per runner. Feature switches and runtime settings live
with the controller (persisted, versioned, audited), change with one command or
API call and take effect in seconds without a redeploy. Infrastructure-as-code
only creates infrastructure (instances, disks, network, IAM) and bootstraps the
controller; it never carries feature toggles or per-runner application
settings. Every new feature ships enabled by default where safe, configurable
on the controller.

Existing per-runner or deploy-only switches are tracked as debt in
[docs/ROADMAP.md](docs/ROADMAP.md#configuration-debt) (see the config-architecture design).

## Changes

- Keep pull requests small and focused; update docs and acceptance criteria
  with the behavior they describe.
- Run `cargo test` and the checks for the area you touch before opening a pull
  request.
- Keep secrets, credentials and deployment identifiers out of commits.

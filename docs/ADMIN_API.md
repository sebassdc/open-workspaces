# Admin API

Every operator operation is an HTTP API call on the controller. The `ow` admin
commands (`ow config`, `ow health`, `ow node-pool`, `ow token`, `ow host
invite|budget|revoke`) are a thin wrapper over it: each builds one request, sends
it, and prints the result. No CLI code reads or writes the controller database,
on the controller host either. Shipped in v0.9.54.

```mermaid
flowchart LR
  M[ow on a laptop] -- "HTTPS + Access JWT or tailnet login" --> G[gateway: POST /api/admin]
  H[ow --local on the host, as root or the controller user] -- "HTTP over admin.sock, peer uid" --> S[controller: POST /api/admin]
  G --> O[one op registry and handlers]
  S --> O
  O --> DB[(catalog.sqlite3: settings, nodes, tokens, admin_audit)]
```

## Who may call it

- **Over HTTPS**, the gateway's designated pool owner only: the verified Access
  JWT subject or tailnet user ID must equal the gateway config's
  `node_owner_subject`, and the catalog must bind `node_owner_user_id` to that
  identity (the gate `/api/hosts/*` has always used). Email never grants
  authority, and pool admins (`pool_admins`, who manage any box) are not admins
  of the controller. Workload tokens reach only `POST /api/operation`.
- **Over `<controller data dir>/admin.sock`** (mode 0600), a peer whose uid is 0
  or the controller's own user. Anyone who passes could already edit the
  database, so the socket adds no power. Only `GET /api/version` and
  `POST /api/admin` are served there. The node controller (`ow host up`) serves
  it, or the dashboard (`ow dashboard`) on a host without one; whichever holds
  `admin.lock` serves, and the other takes over if it stops. With neither
  running, admin commands on the host stop with `controller_unreachable`: there
  is no direct-database fallback, and none is needed to start them again.
- **AWS hosts** (`deploy/aws`): the `dashboardOwner` input names the owner's
  tailnet login; it binds once they have signed in to the dashboard and the
  dashboard has restarted. It also makes them the dashboard's pool owner.

A signed-in user who is not the owner gets `403 admin_required`; the controller
logs an `admin_refused` warning (no table row). One admin; more than one is not
supported.

## How the CLI picks a transport

1. `--local`: `admin.sock` in `--data-dir`, else `OW_DATA_DIR`, else
   `~/.ow/controller` (the directory `ow host up` uses).
2. `--server URL`: HTTPS.
3. `--data-dir` alone: that controller's `admin.sock` (host scripts use this).
4. `OW_SERVER`, then the server saved by `ow login`: HTTPS.
5. Otherwise the default `admin.sock` if it exists, else exit 3 with `no_server`.

```sh
ow config get limits.box_memory_mib                       # from a laptop signed in as the owner
sudo ow --local --data-dir /srv/ow/.ow/controller config list   # on the controller host
```

Exit status: 0 success; 1 an API error, printed as `<error_code>: <error>` on
stderr; 2 a usage error; 3 the server or socket cannot be reached or refuses the
sign-in (`server_unreachable`, `controller_unreachable`,
`authentication_or_admission`, `unsupported_server`, `no_server`). `ow health`
also exits 1 while an alert is active. Warnings in a result go to stderr.

`ow node-join` and `ow node-revoke` remain as hidden aliases of `ow host invite`
and `ow host revoke`. `node-join --controller` is accepted and ignored: the
controller names its own URL (the gateway's `https://HOST/_nodes`, or on the
socket the URL `ow host up` saved).

## Transport and envelope

`POST /api/admin`, every op, reads included. No query string. Over HTTPS send
`content-type: application/json`, `origin: https://<server host>` and
`x-ow-request: dashboard` (the same CSRF guard as `/api/operation`) and the
login (`cf-access-token` for Access, nothing for a tailnet server). Over the
socket send `content-type` and `x-ow-request`; `x-ow-claimed-user: NAME` (the
CLI sends `SUDO_USER`) is recorded as an unverified claim.

The body is a JSON object, at most 16 KiB: `"op"` and that op's fields at the top
level. Unknown fields are refused. Success is HTTP 200
`{"ok": true, "op": NAME, "result": {...}}`; failure is
`{"ok": false, "op": NAME or null, "error_code": CODE, "error": SENTENCE, "details": {...}}`.
Responses carry `cache-control: no-store`.

`GET /api/version` lists `admin_api_v1` in `capabilities` and every op in
`admin_ops`. A CLI that finds no `admin_api_v1` stops with `unsupported_server`.

| HTTP | `error_code` | When |
| --- | --- | --- |
| 400 | `invalid_request` | Not JSON, a missing or wrong-typed field, an unknown field, a query string, a field an op does not take |
| 400 | `unknown_op` | `op` is not in the registry |
| 400 | `unknown_setting` | A config key that does not exist |
| 422 | `invalid_value` | A value fails its grammar or range, or names something invalid; `details: {"key", "reason"}` |
| 401 | (gateway) | No or invalid login; plain text, unchanged |
| 403 | `admin_required` | Signed in but not the owner, or a socket peer with another uid |
| 403 | `same_origin_required` | HTTPS without the right `origin`, or no `x-ow-request` |
| 403 | (plain text) | A workload token |
| 404 | `not_found` | The named node or token does not exist |
| 409 | `version_conflict` | `expected_version` is not current; `details: {"expected", "current"}` |
| 409 | `already_exists` | A name that must be new is taken, or a retried op that hands out a secret once |
| 409 | `operation_key_reused` | The same `operation_key` with a different request |
| 409 | `rejected` | A capacity or spend limit refused a host change (the limit's own message) |
| 413 | `too_large` | Body over 16 KiB |
| 503 | `unavailable` | Database busy for over 5 s, or no workspace catalog for token ops |
| 500 | `internal` | Anything else; the detail goes to the controller log only |

## Audit

Every mutating op that passed the gate writes one `admin_audit` row, success or
failure: time, op, actor, `via` (`https` or `local`), the request, the result
(`ok`, `unchanged` or the error code), the configuration version after a
`config.set`, and for `config.set`, `node.pool.set` and `limits.owner.set` the value
and the previous one. A change and its row commit in one transaction. Reads write nothing.
`ow config history` and the idempotent `token.revoke` read this table, so they
know only changes made through the API (from v0.9.54): a token revoked before
then is `not_found`. The
actor is the verified identity: `{"kind":"access"|"tailnet","subject","email"}`
or `{"kind":"host","uid","claimed_user"}`. Tokens and invitation secrets are
never stored. Each successful change also writes an `admin_change` line to the
serving process's journal (`op`, `key` or the name it changed, `actor_email` or
`actor_uid`, `via`); the node controller also forwards it as an OTLP log, the
dashboard does not yet (the table is the record either way). `config_changes` keeps its rows, with the actor now
`access:EMAIL`, `tailnet:LOGIN` or `host:CLAIMED_USER_OR_UID`.

Ops that hand out a secret once (`token.issue`, `host.invite`) take an
`operation_key` (the CLI's `--operation-key`, or a fresh one it prints and saves
under `~/.config/open-workspaces/operations/`). A retry with the same key and
request returns `409 already_exists` instead of a second secret: revoke and
reissue. Keys live in `admin_operations`.

## Ops

| Command | Op |
| --- | --- |
| `ow config list` | `config.list` |
| `ow config get KEY` | `config.get` |
| `ow config set KEY VALUE [--expect-version N]` | `config.set` (VALUE always sent as a string) |
| `ow config history [KEY] [--limit N]` | `config.history` |
| `ow health` | `health.get` |
| `ow node-pool NODE POOL` | `node.pool.set` |
| `ow token issue NAME --owner EMAIL --pool POOL` | `token.issue` |
| `ow token revoke NAME` | `token.revoke` |
| `ow token list` | `token.list` |
| `ow host invite NODE [--ttl S] [--memory M] [--slots N] [--cpus C] [--output FILE]` | `host.invite` |
| `ow host budget NODE --memory M --slots N --cpus C` | `host.budget` |
| `ow host revoke NODE` | `host.revoke` |
| `ow placement status [--pool P]` | `placement.status` |
| `ow actions status [--pool P]` | `actions.status` |
| `ow actions app-key set --key-file FILE` | `actions.app_key.set` (`-` reads standard input) |
| `ow limits get EMAIL` | `limits.owner.get` |
| `ow limits set EMAIL [--cpus N] [--memory-mib N] [--disk-gib N] [--boxes N]` | `limits.owner.set` |

The examples below are the contract: a test parses every request and response,
and checks that the CLI builds each request exactly.

### config.list

Every setting, typed, defaults filled in, and the configuration version (0
before any change).

<!-- admin:example op=config.list -->
```json
{"op": "config.list"}
```
```json
{"ok": true, "op": "config.list", "result": {"version": 42, "settings": {
  "telemetry.enabled": true, "telemetry.interval_s": 15, "telemetry.forward_endpoint": "",
  "telemetry.logs": true, "telemetry.log_level": "info",
  "alerts.offline_after_s": 60, "alerts.disk_low_gib": 20,
  "dashboard.ui_bundle": "builtin", "limits.box_memory_mib": 0,
  "placement.spill": [], "placement.spill_keep_free_mib": 16384, "placement.spill_keep_free_vcpus": 0,
  "placement.spill_keep_free_disk_gib": 100, "upgrade.worker_handover": true,
  "upgrade.running_boxes": "hibernate"}}}
```

### config.get

<!-- admin:example op=config.get -->
```json
{"op": "config.get", "key": "telemetry.interval_s"}
```
```json
{"ok": true, "op": "config.get", "result": {"key": "telemetry.interval_s", "value": 30, "default": 15, "version": 42}}
```

### config.set

`value` is a string, parsed with the key's grammar (empty means the default), or
the key's typed JSON (a boolean, a whole number or a string). With
`expected_version`, the change is refused with `version_conflict` unless the
configuration is still at that version, even when the value is unchanged. The
check order is: admin gate, request shape, `expected_version`, then the value.

A value equal to the stored one returns `"changed": false` and the current
version: no version bump and no `config_changes` row (an `admin_audit` row with
result `unchanged` is still written), so a retry after an uncertain answer is
always safe. `effective` says when it applies: `next_create` for `limits.*` and `placement.*`,
`next_ack` for `telemetry.*`, `alerts.*`, `upgrade.*` and `network.*` (runners get it with the next
acknowledgement, a few seconds), `next_request` for `dashboard.ui_bundle`.
`warnings` lists what is valid but will not do what it reads like.

<!-- admin:example op=config.set -->
```json
{"op": "config.set", "key": "telemetry.interval_s", "value": "30", "expected_version": 41}
```
```json
{"ok": true, "op": "config.set", "result": {"key": "telemetry.interval_s", "value": 30, "previous": 15,
  "changed": true, "version": 42, "effective": "next_ack", "warnings": [], "audit_id": 118}}
```

<!-- admin:example op=config.set -->
```json
{"op": "config.set", "key": "telemetry.interval_s", "value": 1}
```
```json
{"ok": false, "op": "config.set", "error_code": "invalid_value",
  "error": "telemetry.interval_s: telemetry.interval_s must be within 5..=300.",
  "details": {"key": "telemetry.interval_s", "reason": "telemetry.interval_s must be within 5..=300"}}
```

`placement.spill` takes the string grammar (`pool:node:max_boxes`, comma-separated)
or a list of `{"pool", "node", "max_boxes"}`; both store the same value. The
pool cannot be `default`; its runners must be enrolled and in another pool.

<!-- admin:example op=config.set -->
```json
{"op": "config.set", "key": "placement.spill", "value": [{"pool": "ci", "node": "n2", "max_boxes": 2}]}
```
```json
{"ok": true, "op": "config.set", "result": {"key": "placement.spill", "value": [{"pool": "ci", "node": "n2", "max_boxes": 2}],
  "previous": [], "changed": true, "version": 43, "effective": "next_create", "warnings": [], "audit_id": 119}}
```

<!-- admin:example op=config.set -->
```json
{"op": "config.set", "key": "placement.spill", "value": "ci:n9:2"}
```
```json
{"ok": false, "op": "config.set", "error_code": "invalid_value", "error": "placement.spill: node n9 is not enrolled.",
  "details": {"key": "placement.spill", "reason": "node n9 is not enrolled"}}
```

`network.private_egress` takes its typed list (or the same list as a JSON string);
destinations come back sorted, without duplicates.

<!-- admin:example op=config.set -->
```json
{"op": "config.set", "key": "network.private_egress", "value": [{"pool": "ci", "to": ["10.0.5.17:443"]}]}
```
```json
{"ok": true, "op": "config.set", "result": {"key": "network.private_egress", "value": [{"pool": "ci", "to": ["10.0.5.17:443"]}],
  "previous": [], "changed": true, "version": 44, "effective": "next_ack", "warnings": [], "audit_id": 120}}
```

### config.history

The successful `config.set` changes, newest first; `limit` 1 to 500, default 50.

<!-- admin:example op=config.history -->
```json
{"op": "config.history", "key": "telemetry.interval_s", "limit": 2}
```
```json
{"ok": true, "op": "config.history", "result": {"changes": [
  {"audit_id": 118, "unix_s": 1793750400, "key": "telemetry.interval_s", "value": 30, "previous": 15,
   "version": 42, "actor": {"kind": "tailnet", "subject": "u123", "email": "owner@example.com"}, "via": "https"}]}}
```

### health.get

Fleet health: alerts, per runner its latest telemetry and recent events, and the
controller's own recent events (its local worker, a controller-only host's upgrades)
([TELEMETRY.md](TELEMETRY.md)).

<!-- admin:example op=health.get -->
```json
{"op": "health.get"}
```
```json
{"ok": true, "op": "health.get", "result": {"alerts": [], "nodes": [{"id": "n1", "online": true, "revoked": false,
  "last_seen_unix_s": 1793750400, "last_heartbeat_unix_s": 1793750400, "telemetry_v1": true,
  "telemetry_logs_v1": true, "telemetry": null, "events": []}], "controller": {"events": []},
  "settings": {"version": 42}}}
```

### node.pool.set

Puts a runner in a placement pool ([USERS.md](USERS.md#placement-pools)),
effective for the next create. The same pool again returns `"changed": false`.

<!-- admin:example op=node.pool.set -->
```json
{"op": "node.pool.set", "node": "n1", "pool": "ci"}
```
```json
{"ok": true, "op": "node.pool.set", "result": {"node": "n1", "pool": "ci", "previous": "default", "changed": true}}
```

### token.issue

Issues a [workload token](USERS.md#workload-tokens). The token appears only in
this response.

<!-- admin:example op=token.issue -->
```json
{"op": "token.issue", "name": "ci-broker", "owner": "ci@example.com", "pool": "ci", "operation_key": "k7f2"}
```
```json
{"ok": true, "op": "token.issue", "result": {"name": "ci-broker", "owner": "ci@example.com", "owner_id": 12,
  "pool": "ci", "token": "owt_0000000000000000000000000000000000000000000000000000000000000000",
  "note": "Shown once; store it now."}}
```

<!-- admin:example op=token.issue -->
```json
{"op": "token.issue", "name": "ci-broker", "owner": "ci@example.com", "pool": "ci", "operation_key": "k7f2"}
```
```json
{"ok": false, "op": "token.issue", "error_code": "already_exists", "error": "token ci-broker already exists.",
  "details": {"name": "ci-broker", "issued_unix_s": 1793750400}}
```

### token.revoke

Immediate. A name revoked before returns `"changed": false`; a name never issued
is `not_found`.

<!-- admin:example op=token.revoke -->
```json
{"op": "token.revoke", "name": "ci-broker"}
```
```json
{"ok": true, "op": "token.revoke", "result": {"revoked": "ci-broker", "changed": true}}
```

### token.list

Never the token or its hash.

<!-- admin:example op=token.list -->
```json
{"op": "token.list"}
```
```json
{"ok": true, "op": "token.list", "result": {"tokens": [{"name": "ci-broker", "owner": "ci@example.com",
  "owner_id": 12, "pool": "ci", "created_at": "2026-11-04 15:00:00"}]}}
```

### host.invite

A private, expiring, single-use runner invitation ([HOSTS.md](HOSTS.md)).
Defaults: `ttl` 600 s, `memory` 512 MiB, `slots` 2, `cpus` 2. The CLI writes it
to `--output` (a new file in a private directory) or prints it on a terminal.

<!-- admin:example op=host.invite -->
```json
{"op": "host.invite", "node": "n3", "ttl": 600, "memory": 8192, "slots": 2, "cpus": 2, "operation_key": "k9a1"}
```
```json
{"ok": true, "op": "host.invite", "result": {"node": "n3", "secret": "<64 hex digits, shown once>",
  "expires": 1793751000, "policy": "Trusted shared private pool: ...",
  "controller": "https://workspaces.example.com/_nodes", "memory": 8192, "slots": 2, "cpus": 2}}
```

### host.budget

Durable upper bounds for an enrolled runner; lowering needs its boxes stopped.

<!-- admin:example op=host.budget -->
```json
{"op": "host.budget", "node": "n3", "memory": 16384, "slots": 4, "cpus": 4}
```
```json
{"ok": true, "op": "host.budget", "result": {"node": "n3", "memory": 16384, "slots": 4, "cpus": 4,
  "operator_configuration_required": true}}
```

### placement.status

[Spill](MULTINODE.md#spill) rules and usage, for every pool with a rule (or the
one asked for). `home_nodes` are the pool's own runners; `spill` has one entry
per rule, in rule order; `boxes` and `pending` count that pool's boxes;
`free_memory_after_reserve_mib`, `free_vcpus_after_reserve` and
`free_disk_gib_after_reserve` are the most one more box of the pool may use there
(the usual checks still apply). `state`
is `available`, `full` (cap reached), `reserve` (only the reserve of memory,
vCPUs or disk is left),
`off` (`max_boxes` 0), `no_home` (no live runner in the pool, so nothing
spills), `inert_offline`, `inert_draining`, `inert_revoked` or
`inert_image_missing`.

<!-- admin:example op=placement.status -->
```json
{"op": "placement.status", "pool": "ci"}
```
```json
{"ok": true, "op": "placement.status", "result": {"version": 43, "keep_free_mib": 32768, "keep_free_vcpus": 16,
  "keep_free_disk_gib": 200, "pools": [{"pool": "ci",
  "home_nodes": [{"node": "n1", "online": true, "active": true, "draining": false, "boxes": 5, "pending": 0,
                  "free_slots": 0, "free_memory_mib": 0}],
  "spill": [{"node": "n2", "max_boxes": 2, "boxes": 1, "pending": 0, "box_names": ["ci-runner-7f3a"],
             "free_memory_mib": 57344, "free_memory_after_reserve_mib": 24576, "free_vcpus": 40,
             "free_vcpus_after_reserve": 24, "free_disk_gib": 600, "free_disk_gib_after_reserve": 400,
             "state": "available"}]}]}}
```

### actions.status

Each [runner pool](ACTIONS.md)'s boxes (`launching`, `idle`, `busy`, `removing`,
and how many `spilled` onto another pool's runner), whether it is held after a
failed launch (`held_s`) or its warm boxes wait for room on its own runners
(`home_held_s`), its last error, and the
start times of its latest boxes in ms since the epoch (`create_ms` the create was
sent, `ready_ms` the box booted, `registered_ms` its runner registration was
minted, `started_ms` the runner started, `online_ms` GitHub listed it online,
`busy_ms` GitHub gave it a job, `removed_ms` it was deleted). `app_key` never
holds the key, only whether one is stored and its fingerprint. It is read from the
catalog, so either process serving the API answers it; `running` is false until
the pools' first pass.

<!-- admin:example op=actions.status -->
```json
{"op": "actions.status", "pool": "ci"}
```
```json
{"ok": true, "op": "actions.status", "result": {"running": true, "app_id": 123456,
  "app_key": {"present": true, "fingerprint": "sha256:9f2c41d07a6b33e1"},
  "pools": [{"pool": "ci", "configured": true, "warm": 4, "max": 6, "launching": 0, "idle": 4, "busy": 1,
             "removing": 0, "spilled": 0, "held_s": 0, "home_held_s": 0, "last_error": null,
             "boxes": [{"box": "ci-3f1c9a02b7d4", "phase": "started", "age_s": 312, "online": true, "busy": true,
                        "spilled": false}],
             "recent": [{"box": "ci-3f1c9a02b7d4", "pool": "ci", "create_ms": 1793760000000, "ready_ms": 1793760002100,
                         "registered_ms": 1793760002600, "started_ms": 1793760003400, "online_ms": 1793760005900,
                         "busy_ms": 1793760290000}]}]}}
```

### actions.app_key.set

Stores the GitHub App's private key (PEM) that the pools register runners with,
0600 in the controller's data directory. It is never returned or logged, and
the audit log records the request with the key replaced. Setting the same key
again returns `"changed": false`.

<!-- admin:example op=actions.app_key.set -->
```json
{"op": "actions.app_key.set", "private_key": "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...\n-----END RSA PRIVATE KEY-----\n"}
```
```json
{"ok": true, "op": "actions.app_key.set", "result": {"fingerprint": "sha256:9f2c41d07a6b33e1", "changed": true,
  "audit_id": 88}}
```

### limits.owner.get

One owner's [limits](LIMITS.md): `caps` (their override, or the defaults when
`override` is false) and `usage`, the same four fields counted the way admission
counts them. `owner` is the login email of a user the catalog knows; an unknown
one is `not_found`.

<!-- admin:example op=limits.owner.get -->
```json
{"op": "limits.owner.get", "owner": "someone@example.com"}
```
```json
{"ok": true, "op": "limits.owner.get", "result": {"owner": "someone@example.com", "owner_id": 1,
  "caps": {"cpus": 16, "memory_mib": 32768, "disk_gib": 256, "boxes": 8}, "override": false,
  "usage": {"cpus": 0, "memory_mib": 0, "disk_gib": 0, "boxes": 0}}}
```

### limits.owner.set

Sets one owner's override in the controller's `limits.json`, keeping every other
field, through an atomic, private (0600) replace. Fields left out keep their
current value (from the override, or the defaults); at least one is required.
Each is a whole number; zero denies new demand in that dimension. Lowering a cap
never stops or shrinks a box. It applies from the next create, fork, resize or
restore. The same caps again return `"changed": false`.

<!-- admin:example op=limits.owner.set -->
```json
{"op": "limits.owner.set", "owner": "someone@example.com", "cpus": 26, "memory_mib": 53248, "disk_gib": 416, "boxes": 13}
```
```json
{"ok": true, "op": "limits.owner.set", "result": {"owner": "someone@example.com", "owner_id": 1,
  "caps": {"cpus": 26, "memory_mib": 53248, "disk_gib": 416, "boxes": 13},
  "previous": {"cpus": 16, "memory_mib": 32768, "disk_gib": 256, "boxes": 8},
  "changed": true, "effective": "next_create"}}
```

### host.revoke

Revoked IDs cannot be reused. A runner already revoked returns
`"changed": false`.

<!-- admin:example op=host.revoke -->
```json
{"op": "host.revoke", "node": "n3"}
```
```json
{"ok": true, "op": "host.revoke", "result": {"revoked": "n3", "changed": true}}
```

## Commands that are not ops

Workspace commands (`create`, `exec`, `list`, ...) use `POST /api/operation`
([AGENT_CLI.md](AGENT_CLI.md)). These stay local by nature:

| Commands | Why |
| --- | --- |
| `host up\|start\|stop\|status\|doctor\|run\|join\|configure\|update-assets\|rollback-assets`, `up`, `down` | Install, start, stop or repair the machine and the server itself; the API cannot run them while it is the thing being started or fixed. |
| `node-controller`, `node-agent`, `dashboard`, `serve`, `worker`, `supervisor`, `gateway` | Long-running server processes, not operations. |
| `backup snapshots\|recover\|restore\|list`, `image build\|pull`, `mac-host` | Act on this machine's own runner data and KVM, not controller state. |
| `backup catalog\|catalog-check` | Disaster-recovery tools that must work with the gateway down; the copy holds every credential hash. |
| `login` | Client-side sign-in. |
| `upgrade` | Replaces this machine's own client with GitHub's latest release, checked by its own `status` against the controller, or the release `GET /api/version` names. |

These will become ops, each with its contract in its own change: `scaler`,
`dashboard-ui list|install`, `node list|add`, `image publish`, `image list
--catalog` (next), then `node drain`, `store gc|verify|undo-plan` and `image
build --publish-to` (which need long-running jobs). Until then they run on the
controller host as before.

A test mirrors these lists: it fails when a command of `ow` is in none of them
(an op, a workspace command, a local one with its reason, or a planned op), or
when an op has no command, no documented example, or no CLI case that builds its
example exactly. `image list` counts as a workspace command; its `--catalog`
form is one of the planned ops.

# Host-core implementation evidence (evolving)

Updated: 2026-10-03. Release, independent final acceptance and guest smoke remain pending.

## Repeated unit failure and causal correction

Failing test: `nodes::tests::public_aliases_share_native_generation_and_clamp_forged_capacity`.
The repeated serial result was **27 passed, 1 failed, 3 ignored**. The failure
was the fixture's final `fs::remove_dir_all(root).unwrap()`: an I/O
`DirectoryNotEmpty` error (Linux ENOTEMPTY), after protocol assertions completed.
The original raw panic output is not retained in this report; do not treat this
description as a verbatim panic transcript or infer a stable source line number.

Cause: closing/revoking fixture WebSockets leaves detached controller tasks
finishing generation-scoped SQLite cleanup. Session-map removal occurs before
that final database write. Deleting the disposable database directory at that
point races creation of SQLite auxiliary files. Waiting for an empty session map
alone did not remove the race, explaining the repeated failure after that first
attempted fixture correction. This was fixture teardown, not a failed alias,
CPU-clamp, generation-replacement or revocation assertion.

Correction in `crates/ow/src/node_tests.rs`: require drained sessions, abort the
fixture listener, then retry deletion of **only this fixture root**, only on
`DirectoryNotEmpty`, for at most two seconds. Other errors fail immediately.
No controller admission, authentication, lock or runtime behavior was weakened.

Latest current-source serial suite after that correction: **28 passed, 0 failed,
3 ignored**. Earlier passing runs do not substitute for this result. Before
final acceptance, retain a fresh textual focused-fixture/serial result with the
frozen source identity.

## Runtime and artifacts

No host-core VM smoke, managed worker startup or guest execution has occurred.
The existing Ubuntu worker/VMM and old experiment evidence remain untouched.
Read-only admission inventory resolves one live 2048-MiB guest and no unresolved
live/registered demand; immediate admission must be repeated before any guest.

`release-v1` and `host-public-v1` in ignored dedicated build storage are
superseded: they lack subsequent `host.rs`, `main.rs` and `onboarding.rs`
changes. They are not final accepted or published artifacts. Freeze source,
rebuild into new outputs, compare source/build/copied CLI identities and rerun
artifact checks before final independent acceptance. No public deployment,
cloud writes, existing-service replacement or push has occurred.

## Superseded final receipt and bounded stop correction

The first `smoke-final` attempt failed at HTTPS asset download with
`invalid peer certificate: ... CaUsedAsEndEntity`: its disposable TLS fixture
used a CA as the server certificate. No managed worker/VM started. The scratch
controller exited, edge closed and baseline Ubuntu VMM identity/demand matched
before/after in private `smoke-final/receipt.json`. The fixture now uses a separate
CA-signed leaf: CA:FALSE, serverAuth, SAN localhost/127.0.0.1. TLS validation stays
strict.

Immediate admission found four unreadable same-UID cwd links. Independent
systemd/logind records bind them to the user manager, its PAM child and two
root-supervised Tailscale SSH session leaders. Exact private PID/start/cgroup
proof is retained in `admission-immediate.json`; no broad ignored category.
Verifier independently observed that shipped stop would reject these ordinary
supervisors even after owned worker exit. Therefore `release-final` and
`host-public-final` and their ready receipt are **superseded**, pending restaging.

Bounded source correction: unreadable cwd is accepted only when independently
bound to systemd user-manager MainPID + init.scope + parent + exact launch, its
PAM child in that same scope, or root-supervised tailscaled MainPID + SSH-wrapper
launch + logind exact session leader/service/scope/UID. Query failures, mismatches,
unknown processes and unreadable ordinary mocks remain UNKNOWN. Queries use
fixed root-owned system tools, cleared environment, bounded time/output and no
shell. All readable guest cwd evidence is still checked. Actual unmodified CLI
stop must execute in the next smoke; harness exemptions cannot establish it.

## Frozen final2 receipt and executed core slice

Authoritative private outputs under
`/home/sebassdc/dev/open-workspaces/data/host-core-build/`:

- `ready-receipt-final2.json`; `release-final2/build.json` and static CLI/guest pair.
- `host-public-final2/`: immutable fixed five-file clean Alpine bundle, copied
  matching Linux CLI/checksum and build manifest. Previous v1/final outputs are
  superseded and retained; none has been publicly selected or deployed.
- `focused-fixture.log` (1 passed), `restaged-serial-units.log` (28 passed,
  0 failed, 3 ignored), `restaged-source-checks.log` (6 PTY cancellation cases,
  cleared-HOME root, 2 pre-network owner-output denials and 10 offline ingress
  checks), `restaged-clippy.log` (passes with existing baseline style allowances
  collapsible_if/needless_borrow/cmp_owned).
- `smoke-final4/receipt.json`, `smoke-final4/guided-join.log`, native restart and
  two actual CLI stop logs; `cleanup-final.json` records final owned-root removal.

Compiled input manifest SHA256:
`c8506e396237bff37b0a546871203ad14a122f2419c55376d4406c48585b2068`.
Static Linux CLI SHA256:
`c67635c050509feae3a5f2c11ede32dde394061cf6982acdf967f1c7d66bfe89`.
Matching guest agent SHA256:
`1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7`.
Build script checks source hashes before/after build; final compiled files still
match that manifest. The publisher checks pins, freshly prepared clean-image
recipe/digest, CLI/guest build digests and copied CLI help/hash before atomic
publication into ignored storage. This is local staging, not public publication.

Core slice `smoke-final4` passed against these exact copied/installed bytes:
clean per-user install outside the repository; hidden guided invitation with
512 MiB / 2 slots / 2 CPU; validated CA-signed TLS leaf; real prefixed controller
acknowledgement; real 256-MiB/1-vCPU Alpine create/exec through the outbound node
proxy; cold guest stop/start and disk persistence; managed worker stop/start,
native TLS reconnect with the same fixture credential and saved CA after removing
the original input CA; persisted disk still readable. Both actual unmodified
`host stop` executions returned zero and confirmed released owned worker/agent
locks and absence of owned guest processes, while known supervisors stayed live.
The negative held-worker-lock/missing-socket and stale-ack cases remain in the
passing unit suite; independent unresolved-process V3 remains verifier-owned.

Admission included actual live prototype catalog and enrolled a/b, all direct
worker statuses/lists, actual VMM/process identity, held writer locks, historical
roots and uncertain operations. Stale experiment evidence was preserved. One
new static 512-MiB/2-slot/2-CPU worker and one 256-MiB guest were used: peak two
guests/2304 MiB including Ubuntu, below four/3072 and new-guests three/768.
Measured available RAM stayed about 24 GiB. The isolated controller/edge were
loopback fixtures; existing a/b and native 8790 service were never repointed,
restarted, signaled or replaced. Native/prefix compatibility was executed on
one disposable controller/credential; existing a/b reconnect during rollout is
still planner/verifier acceptance, not a core live-service test.

Earlier `smoke-final2`/`smoke-final3` setup attempts also failed before any worker
or VM startup. The latter exposed Python strict CA validation's missing CA
keyUsage extension; the final fixture adds critical CA:TRUE/keyCertSign/cRLSign
and a separate CA:FALSE/serverAuth/SAN leaf. No TLS bypass was introduced.
All four scratch controllers exited; all eight precisely recorded scratch
host/controller roots were removed only after released locks and independent
owned-process absence. Private receipts/logs and final build outputs remain.
No live test worker, guest, controller, agent or helper remains. Ubuntu is still
the sole guest at 2048 MiB, with the same VMM PID/start ticks before and after.
Planner independently inspected final4 receipt and repeated resolved admission.

**Core guest turn released; no further core guests.** Verifier owns the exclusive
turn now. Product source stays frozen through verification. Independent final
V1–V6, guest PTY/human gateway acceptance, actual cloud state/capture/apply/rollback,
actual gateway-selected public CLI hash and physical second-host/NAT remain
pending. No cloud/public/service write or push occurred. Planner owns release.

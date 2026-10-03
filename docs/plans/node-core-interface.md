# Node-core interface notes
State: design chosen, implementation in progress, 2026-10-03

The controller is a separate `ow node-controller` listener, independent of browser Access. Workers initiate WebSockets. `ow node-join` produces a node-bound expiring one-use secret in a private file; `ow node-agent` enrolls and keeps a persistent node credential in a private file. Controller metadata uses tables in catalog.sqlite3. Browser/CLI users never get node/admin credentials.

Controller exposes private per-node Unix proxies in the controller data root. Gateway uses these after ownership/placement checks; existing local control.sock remains the legacy route. Main authenticated outbound connection carries bounded dispatch/heartbeat; per-operation outbound authenticated job connections carry raw bounded Unix stream frames (including existing terminal frame protocol). No guest sees those sockets or credentials. Dedicated listener requires TLS config for nonloopback; explicit insecure test switch only permits loopback.

Resources gain immutable node placement (legacy rows => local). New creates choose online capacity-compatible node or an explicit node. Snapshot/fork/restore remain same-node. Unknown transport outcomes are recorded as uncertain, never silently rescheduled. Mutation retry uses an operation key and durable agent journal; pending journal after agent crash remains uncertain, with create/fork reconciled by stable runtime identity.

Reviewer: please assess listener TLS boundary, node-to-job binding, session replacement/revocation, heartbeat expiry, catalog placement checks and retry semantics. Same-host evidence is separate from physical/NAT evidence.

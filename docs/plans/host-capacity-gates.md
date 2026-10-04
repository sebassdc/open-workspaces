# Host capacity — planner decision

**NEED:** Confirm conservative owner-decrease semantics: fresh online idle inventory, zero reservations and no pending/uncertain effects; offline decreases reject.
**WHY NOW:** An offline channel cannot prove stopped guests. Local stopped-host configuration is separate from controller authorization.
**CONTEXT:** Increases update only durable owner ceilings. The worker retains its chosen consent budget. Local configure/update requires released owned locks, no socket/agent/guest processes, and a stopped recovery journal. No command stops guests as a side effect.
**IF YES:** Test online-idle decreases and fail-closed offline/unknown demand. Operator explicitly stops, configures and starts.
**IF NO:** Keep decreases disabled until an authenticated stopped-inventory receipt protocol is defined; raising and local consent work can finish.
**DEFAULT:** “ok” approves the conservative rule above.

Resolved by planner: approve the conservative rule above, including extra node_usage RAM/CPU/slots. Sequence owner decrease while online idle → host stop → configure → start. Offline decreases intentionally reject. No owner decision remains.

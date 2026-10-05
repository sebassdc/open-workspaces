# Guest SSH — planner review and handoff
Updated: 2026-10-04 | Size: L | State: source reviewed and integrated; distribution/deployment pending

## Result
Builder commit a38a635 merged as f1ab6ad. Read docs/SSH.md, ssh-guest-contract.md and ssh-guest-report.md. Actual guest OpenSSH, key enrollment/revocation, binary ProxyCommand and owner-authorized fixed-port routing are implemented. The live installer/gateway/controller/workers still use the previous releases: no SSH deployment or existing guest upgrade was performed.

## Reviewed evidence
Parent read SSH/client/gateway/guest policy, ownership/node/runtime integrations, lifecycle/logging and provisioning diff, plus report/handoff. Independently rehashed the frozen native binary, every compiled/fixture source input against committed blobs and all three final receipts. Native binary SHA38a6303328ebdb718efa45963cfb8f83241f306880b294ff79ecf0cc19d3a479; input map cecd172d42c11892125eb7be04b26b91632330ecdb5cdd498a375b8d742b39b3. Ubuntu23/Arch23/Alpine5 executed real checks; serialized offline49 passed/5ignored. Earlier parallel recovery flake and failed iterations remain explicitly documented. Parent did not rerun guests or tests.

Private before/after capacity manager status confirms identical shared channel descriptors, worker status, owner VMM PID/start ticks and disk inode/size. Builder final scan reports zero fixture helpers. Evidence under isolated lane data/ssh-evidence and v/v,w,x is ignored and contains private fixture keys; never publish it. Host turn released.

## Scope and limits
Reviewed source follows owner-first lookup, fixed guest endpoint, no TLS redirects, generation/key fences and external snapshot-safe policy; actual SSH/SFTP/PTY and denial cases establish the bounded Linux slice. Native GNU debug evidence is not static CLI distribution, public Cloudflare/physical-host/editor acceptance or Mac hosting. New image recipe was changed but no image built/published. Old guests require explicit opt-in ssh-authorize --upgrade; package deadlines can leave an uncertain operation. No automatic changed-key trust or host-key rotation. Guest root can change its own server. Long-duration/exhaustion/private-forwarding and diagnostic disk-full tests are documented exclusions.

## Next step
Prepare a separately reviewed portable CLI/gateway/controller/worker release with pinned artifacts, safe existing-host upgrade instructions and raw/compressed bundle compatibility before deployment. Do not ask owners to run new SSH commands with the currently published CLI. Preserve original guests; no automatic guest mutation or shared worker restart. Mac builder can use additive guest_ssh_v1 and SSH ops contract now available in main; missing capability denies remote SSH.

Pending management files were preserved, including README via a temporary path-scoped stash restored after cherry-pick. No infrastructure writes, paid services or owner guest actions. Source push is authorized by owner's request to make main available to the Mac agent.

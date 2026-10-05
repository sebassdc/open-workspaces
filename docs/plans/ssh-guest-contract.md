# Guest SSH implementation contract
Size: L | Pattern: ADR0002 ownership, fixed placement, fenced outbound jobs.
State: source implementation authorized; real guest turn requires planner grant.

```mermaid
flowchart LR
 C[OpenSSH client]:::new -->|binary stdio| P[ow ssh-proxy]:::new
 P -->|authenticated verified WSS| G[Owner gateway]:::old
 G -->|private fixed placement| N[Fenced outbound node bridge]:::old
 N -->|private ssh request then bytes| W[Worker]:::new
 W -->|fixed TCP port 22| S[Guest OpenSSH dev]:::new
 classDef old fill:#eee,stroke:#aaa,color:#777
 classDef new fill:#dfd,stroke:#282,stroke-width:3px
```
Grey is existing authority; green is new SSH behavior.

1. Add private `ssh` stream operation (JSON acknowledgement then raw bytes),
   WSS `/api/ssh/NAME` binary frames <=16KiB, 16 sessions, 5s writes,
   90s inactivity, 1h maximum/JWT expiry. No requested address or port.
2. Add explicit per-machine owner key replacement/revocation (`ssh-keys`) and
   public host identity inspection (`ssh-info`). Ed25519 public keys only,
   <=16 keys; reject options, certificates and private material. Private keys
   stay client-side. Sidecar policy outside snapshots overrides restored keys.
3. Guest package is ordinary distribution OpenSSH. New Ubuntu/Arch templates
   have versioned `/etc/ow-ssh-v1`; legacy guests need explicit `--upgrade`.
   dev only, passwords/root/agent/X11/remote forwarding disabled. Local TCP
   forwarding supports editors but obeys host-enforced guest egress policy.
4. Kill inherited sshd/session processes on fork/restore before TAP exposure.
   Fork removes managed authorization and host keys. New child enrollment
   receives its own identity and host key. Same-machine host key is pinned in
   private sidecar; rollback to a different/missing key fails closed. No
   automatic trust replacement. Key replacement fences existing byte streams.
5. Implement OpenSSH launcher, binary proxy and reviewable config output;
   leave ~/.ssh/config untouched. Offline tests first; dedicated guest tests
   only after planner grants exclusive bounded host turn.

Blast radius: isolated source, new ignored fixtures only. Existing owner guests,
services, image pins and published bundles remain untouched.
Done: actual SSH exec/PTY/exit/SFTP through both routes plus TLS/owner/fence/key/
identity checks and cleanup evidence. Mocks establish transport only.
Gates: AUTHORITY, IDENTITY, RESOURCES, RELEASE as assigned brief.
Shared capability: optional `guest_ssh_v1:true` means worker understands the fixed
stream plus key/info operations and external snapshot-safe key policy. Missing
means unsupported. It does not promise every installed image contains sshd.
No protocol version or catalog migration; backend-independent semantics.

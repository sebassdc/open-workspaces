// In-memory stand-in for the runner worker's control.sock, modelled on the
// Rust test fakes (crates/ow/src/catalog_tests.rs): newline-delimited JSON
// requests, one reply line, and for "terminal" a framed byte stream after it
// (kind u8, length u32 BE, bytes; kind 0 data, 1 resize, 2 exit code).
import crypto from 'node:crypto';
import net from 'node:net';

const GIB = 1024 ** 3, MAX_MEMORY_MIB = 4096;
// Every capture of a machine with exactly this many vCPUs fails, so tests can see a failed snapshot.
export const FAILING_CAPTURE_VCPUS = 13;

export function fakeWorker(socketPath, log = () => {}) {
  const machines = new Map(), snapshots = new Map();
  const handlers = {
    list: () => [...machines.values()],
    snapshots: () => [...snapshots.values()],
    // The subset of the real worker's "status" (crates/ow-core/src/runtime.rs) that admission reads.
    status: () => ({
      runtime: 'fake', running: [...machines.values()].filter(m => m.state === 'running').length, workspace_count: machines.size,
      snapshot_delete_v1: true, disk_resize_v1: true, max_disk_gib: 16384, reserved_disk_bytes: 0, unwritten_disk_bytes: 0,
      free_disk_bytes: 4096 * GIB, available_disk_bytes: 4096 * GIB, disk_observed_unix_s: Math.floor(Date.now() / 1000), min_free_gib: 0,
      max_running: 16, max_memory_mib: MAX_MEMORY_MIB, max_vcpus: 16, max_workspaces: 64, max_guest_memory_mib: 16384, max_guest_vcpus: 16,
      max_snapshots: 64, images: ['alpine', 'arch', 'ubuntu'], internet: true, prototype: true,
    }),
    stats: () => ({running: Object.fromEntries([...machines.values()].filter(m => m.state === 'running').map(m => [m.id, {pss_kib: m.memory_mib * 256}])), reserved_memory_mib: 0}),
    create: r => {
      // Like the real runner: a repeated create with the same shape returns the machine, any other is refused.
      const existing = machines.get(r.id);
      if (existing) {
        if (existing.memory_mib !== (r.memory_mib ?? 256) || existing.vcpu_count !== (r.vcpu_count ?? 1) || existing.image !== (r.image ?? 'alpine') || (r.disk_gib != null && existing.disk_bytes !== r.disk_gib * GIB)) throw Error('ID already exists with different parameters');
        return existing;
      }
      admit(r.memory_mib ?? 256);
      const m = {id: r.id, memory_mib: r.memory_mib ?? 256, vcpu_count: r.vcpu_count ?? 1, image: r.image ?? 'alpine', image_digest: r.image_digest ?? null, state: 'running', source: null, disk_bytes: (r.disk_gib ?? 32) * GIB, disk_used_bytes: 64 * 1024 * 1024};
      machines.set(r.id, m);
      return m;
    },
    start: r => {
      if (get(r.id).state !== 'running') admit(get(r.id).memory_mib);
      return Object.assign(get(r.id), {state: 'running'});
    },
    hibernate: r => Object.assign(get(r.id), {state: 'hibernated'}),
    stop: r => ({workspace: Object.assign(get(r.id), {state: 'stopped'})}),
    resize: r => {
      const m = get(r.id);
      for (const key of ['memory_mib', 'vcpu_count']) if (r[key] != null) m[key] = r[key];
      if (r.disk_gib != null) m.disk_bytes = r.disk_gib * GIB;
      return m;
    },
    snapshot: r => {
      const m = get(r.id);
      if (m.vcpu_count === FAILING_CAPTURE_VCPUS) throw Error('capture failed');
      const s = {name: r.name, workspace: r.id, memory_mib: m.memory_mib, vcpu_count: m.vcpu_count, image: m.image, state: 'ready'};
      snapshots.set(r.name, s);
      return s;
    },
    'snapshot-delete': r => {
      const existed = snapshots.get(r.name)?.workspace === r.id;
      if (existed) snapshots.delete(r.name);
      return {deleted: r.name, workspace: r.id, existed};
    },
    restore: r => Object.assign(get(r.id), {state: 'running'}),
    fork: r => {
      // Forks of a 3 vCPU parent are refused as the real worker refuses one on a full runner.
      if (get(r.id).vcpu_count === 3) throw Error('CPU reservation limit (16 vCPUs) reached');
      const child = {...get(r.id), id: r.child, source: r.id, state: 'running'};
      machines.set(r.child, child);
      // Like the real worker, a fork of the running machine keeps its own capture, named for no one.
      if (!r.snapshot) {
        const name = `fork-${crypto.randomBytes(12).toString('hex')}`;
        snapshots.set(name, {name, workspace: r.id, fork: r.child, memory_mib: child.memory_mib, vcpu_count: child.vcpu_count, image: child.image, state: 'ready'});
      }
      return child;
    },
    delete: r => {
      const gone = [...snapshots.values()].filter(s => s.workspace === r.id).map(s => s.name);
      // A machine whose creation failed was never registered; deleting it cleans up and succeeds.
      if (!machines.has(r.id)) return {deleted: r.id, snapshots: [], registered: false};
      if (get(r.id).state === 'running') throw Error('workspace is running; stop or hibernate it first');
      machines.delete(r.id);
      for (const name of gone) snapshots.delete(name);
      return {deleted: r.id, snapshots: gone};
    },
    // Guest output is whatever the command says, so tests can check it is shown as text.
    // A real guest command past 30 s ends with this worker diagnostic.
    exec: r => {
      // What the real worker says when a guest command runs past 30 s.
      if (r.command.startsWith('sleep ')) throw Error('guest command timed out after 30s');
      return {output: r.command.replace(/^echo /, ''), exit_code: r.command.startsWith('false') ? 1 : 0};
    },
  };
  // Like the real worker, refuse to run more guest memory than the host has.
  function admit(memory) {
    const running = [...machines.values()].filter(m => m.state === 'running').reduce((sum, m) => sum + m.memory_mib, 0);
    if (running + memory > MAX_MEMORY_MIB) throw Error(`insufficient host memory for ${memory} MiB`);
  }
  function get(id) {
    const m = machines.get(id);
    if (!m) throw Error(`no workspace ${id}`);
    return m;
  }

  const server = net.createServer(socket => {
    let pending = Buffer.alloc(0), terminal = false;
    socket.on('error', () => {});
    socket.on('data', chunk => {
      pending = Buffer.concat([pending, chunk]);
      if (terminal) return frames();
      const end = pending.indexOf(10);
      if (end < 0) return;
      const request = JSON.parse(pending.subarray(0, end).toString());
      pending = pending.subarray(end + 1);
      log(request);
      if (request.op === 'terminal') {
        get(request.id);
        terminal = true;
        socket.write(JSON.stringify({ok: true, result: {}}) + '\n');
        send(0, Buffer.from('fake-shell$ '));
        return frames();
      }
      let reply;
      try { reply = {ok: true, result: (handlers[request.op] ?? (() => ({})))(request)}; }
      catch (error) { reply = {ok: false, error: error.message}; }
      socket.end(JSON.stringify(reply) + '\n');
    });
    function send(kind, bytes) {
      const header = Buffer.alloc(5);
      header[0] = kind; header.writeUInt32BE(bytes.length, 1);
      socket.write(Buffer.concat([header, bytes]));
    }
    // A cooked-mode echo shell: typed bytes come back; "exit" ends with code 0.
    let line = '';
    function frames() {
      while (pending.length >= 5 && pending.length >= 5 + pending.readUInt32BE(1)) {
        const kind = pending[0], bytes = pending.subarray(5, 5 + pending.readUInt32BE(1));
        pending = pending.subarray(5 + bytes.length);
        if (kind !== 0) continue;
        for (const ch of bytes.toString()) {
          if (ch !== '\r') { line += ch; send(0, Buffer.from(ch)); continue; }
          if (line.trim() === 'exit') { send(2, Buffer.from([0, 0, 0, 0])); socket.end(); return; }
          send(0, Buffer.from(`\r\n${line}\r\nfake-shell$ `)); line = '';
        }
      }
    }
  });
  return new Promise((resolve, reject) => server.once('error', reject).listen(socketPath, () => resolve(server)));
}

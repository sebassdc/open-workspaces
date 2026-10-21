// Runs a real `ow dashboard` (tailnet sign-in) against a fake worker, behind a
// local TLS proxy that plays `tailscale serve`. Each browser context picks its
// tailnet user with the `ow-e2e-user` cookie, so tests never share machines.
import {spawn, execFileSync} from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import {DatabaseSync} from 'node:sqlite';
import {fileURLToPath} from 'node:url';
import {fakeWorker} from './fake-worker.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');
const ow = process.env.OW_BIN || path.join(repo, 'target/debug/ow');
const proxyPort = Number(process.env.OW_E2E_PORT || 18443), dashboardPort = proxyPort + 1;
export const HOSTNAME = 'dashboard.e2e.ts.net';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ow-e2e-'));
fs.chmodSync(root, 0o700);
const data = path.join(root, 'data'), bin = path.join(root, 'bin');
fs.mkdirSync(data, {mode: 0o700});
// The marker `ow` writes when it adopts an empty data directory (crates/ow/src/host.rs).
fs.writeFileSync(path.join(data, '.ow-data'), 'open-workspaces local prototype v1\n', {mode: 0o600});
// A wholly self-hosted pool (docs/LIMITS.md), so the owner may invite hosts.
fs.writeFileSync(path.join(data, 'limits.json'), JSON.stringify({pool: {hourly_cents: {'self-hosted': 0}, default_type: 'self-hosted'}}), {mode: 0o600});
// Per-box AWS access with one allowed role (docs/AWS_ACCESS.md); nothing here reaches AWS.
fs.writeFileSync(path.join(data, 'aws-identity.json'), JSON.stringify({issuer: `https://${HOSTNAME}`, allowed_roles: [{role_arn: 'arn:aws:iam::111122223333:role/open-workspaces/e2e-reader'}]}), {mode: 0o600});
fs.mkdirSync(bin);
fs.copyFileSync(path.join(here, 'tailscale'), path.join(bin, 'tailscale'));
fs.chmodSync(path.join(bin, 'tailscale'), 0o755);
const config = path.join(root, 'dashboard.json');
// The `owner` test user (whois ID 1001, the first catalog user) is the host pool owner;
// `pooladmin` is a pool admin.
fs.writeFileSync(config, JSON.stringify({auth: 'tailscale', hostname: HOSTNAME, allowed_capability: 'example.com/cap/open-workspaces', upstream: 'http://127.0.0.1:9/', node_owner_subject: '1001', node_owner_user_id: 1, pool_admins: ['pooladmin@example.test']}), {mode: 0o600});
execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-subj', `/CN=${HOSTNAME}`, '-keyout', path.join(root, 'tls.key'), '-out', path.join(root, 'tls.crt')], {stdio: 'ignore'});

const whois = path.join(root, 'whois.json'), users = new Map();
function peer(name) {
  if (!users.has(name)) {
    const n = users.size + 1;
    users.set(name, {ip: `127.1.${n >> 8}.${n & 255}`, id: 1000 + n, login: `${name}@example.test`});
    const table = Object.fromEntries([...users.values()].map(u => [u.ip, u]));
    fs.writeFileSync(whois + '.tmp', JSON.stringify(table));
    fs.renameSync(whois + '.tmp', whois);
  }
  return users.get(name);
}
peer('owner');

const verbose = process.env.OW_E2E_VERBOSE;
const worker = await fakeWorker(path.join(data, 'control.sock'), request => verbose && console.log('worker', JSON.stringify(request)));

// Test-only: commit a placement for one of a user's machines in the real catalog, as a move
// between runners leaves it (managed, at a fresh epoch), so tests can drive a moved machine.
function manage(login, name) {
  const db = new DatabaseSync(path.join(data, 'catalog.sqlite3'));
  try {
    db.exec('PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON; BEGIN IMMEDIATE');
    const box = db.prepare("SELECT r.owner,r.physical,r.metadata FROM resources r JOIN users u ON u.id=r.owner WHERE u.email=? AND r.kind='machine' AND r.name=?").get(login, name);
    if (!box) throw Error(`no machine ${name}`);
    db.prepare("INSERT INTO settings(key,value) VALUES('transfer_epoch','0') ON CONFLICT(key) DO NOTHING").run();
    const epoch = Number(db.prepare("SELECT value FROM settings WHERE key='transfer_epoch'").get().value) + 1;
    db.prepare("UPDATE settings SET value=? WHERE key='transfer_epoch'").run(String(epoch));
    const meta = JSON.parse(box.metadata);
    const intent = {kind: 'move', machine: name, target: 'local', capture: null, child: null, snapshot: null, memory_mib: meta.memory_mib, cpus: meta.vcpu_count, disk_bytes: meta.disk_bytes, accept_cold: false};
    const operation = db.prepare("INSERT INTO operations(owner,op,resource,state,node,retry_key,request,finished_at) VALUES(?,'move',?,'succeeded','local',?,?,CURRENT_TIMESTAMP)").run(box.owner, name, `e2e-move-${epoch}`, JSON.stringify({transfer: intent})).lastInsertRowid;
    const transfer = db.prepare("INSERT INTO transfers(operation,kind,source,machine,source_node,source_epoch,source_policy_generation,target,epoch,step) VALUES(?,'move',?,?,'local',0,0,'local',?,'succeeded')").run(operation, box.physical, box.physical, epoch).lastInsertRowid;
    db.prepare("UPDATE resources SET placement_managed=1,placement_epoch=?,ssh_policy='\"disabled\"' WHERE kind='machine' AND physical=?").run(epoch, box.physical);
    db.exec('COMMIT');
    worker.manage(box.physical, {epoch, node: 'local', transfer: `transfer-${transfer}`});
  } catch (error) {
    if (db.isTransaction) db.exec('ROLLBACK');
    throw error;
  } finally {
    db.close();
  }
}
const dashboard = spawn(ow, ['--data-dir', data, 'dashboard', '--config', config, '--listen', `127.0.0.1:${dashboardPort}`], {
  env: {...process.env, PATH: `${bin}:${process.env.PATH}`, OW_E2E_WHOIS: whois},
  stdio: ['ignore', 'pipe', 'inherit'],
});
const cleanup = () => { dashboard.kill(); fs.rmSync(root, {recursive: true, force: true}); };
process.on('exit', cleanup);
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(0));
dashboard.on('exit', code => { console.error(`ow dashboard exited (${code})`); process.exit(1); });
await new Promise(resolve => dashboard.stdout.on('data', chunk => { process.stdout.write(chunk); if (/listening/.test(chunk)) resolve(); }));

// Like `tailscale serve`: replace client-sent identity headers with the peer's own.
function identity(headers) {
  const cookie = /(?:^|;\s*)ow-e2e-user=([A-Za-z0-9_-]+)/.exec(headers.cookie || '');
  const user = peer(cookie ? cookie[1] : 'owner');
  const clean = Object.fromEntries(Object.entries(headers).filter(([k]) => !/^(tailscale-user-|x-forwarded-for$)/i.test(k)));
  return {...clean, 'tailscale-user-login': user.login, 'x-forwarded-for': user.ip};
}
const proxy = https.createServer({key: fs.readFileSync(path.join(root, 'tls.key')), cert: fs.readFileSync(path.join(root, 'tls.crt'))}, (request, response) => {
  if (request.method === 'POST' && request.url === '/__e2e/manage') {
    let body = '';
    request.on('data', chunk => body += chunk);
    request.on('end', () => {
      try {
        manage(identity(request.headers)['tailscale-user-login'], JSON.parse(body).name);
        response.writeHead(204).end();
      } catch (error) { response.writeHead(400).end(error.message); }
    });
    return;
  }
  const upstream = http.request({host: '127.0.0.1', port: dashboardPort, method: request.method, path: request.url, headers: identity(request.headers)}, reply => {
    if (reply.statusCode >= 500) console.error(`proxy: ${request.method} ${request.url} -> ${reply.statusCode}`);
    response.writeHead(reply.statusCode, reply.headers);
    reply.pipe(response);
  });
  upstream.on('error', () => { response.writeHead(502); response.end(); });
  request.pipe(upstream);
});
proxy.on('upgrade', (request, client, head) => {
  const upstream = net.connect(dashboardPort, '127.0.0.1', () => {
    const lines = Object.entries(identity(request.headers)).map(([k, v]) => `${k}: ${v}`);
    upstream.write(`${request.method} ${request.url} HTTP/1.1\r\n${lines.join('\r\n')}\r\n\r\n`);
    upstream.write(head);
    client.pipe(upstream).pipe(client);
  });
  for (const end of [client, upstream]) end.on('error', () => { client.destroy(); upstream.destroy(); });
});
// Sign the owner in first so it becomes catalog user 1, as configured above.
const first = await new Promise((resolve, reject) => http.get({host: '127.0.0.1', port: dashboardPort, path: '/api/state', headers: identity({host: HOSTNAME})}, resolve).on('error', reject));
if (first.statusCode !== 200) throw Error(`owner sign-in failed: ${first.statusCode}`);
first.resume();
proxy.listen(proxyPort, '127.0.0.1', () => console.log(`e2e dashboard ready on https://${HOSTNAME} -> 127.0.0.1:${proxyPort}`));

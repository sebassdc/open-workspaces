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
fs.mkdirSync(bin);
fs.copyFileSync(path.join(here, 'tailscale'), path.join(bin, 'tailscale'));
fs.chmodSync(path.join(bin, 'tailscale'), 0o755);
const config = path.join(root, 'dashboard.json');
// The `owner` test user (whois ID 1001, the first catalog user) is the host pool owner.
fs.writeFileSync(config, JSON.stringify({auth: 'tailscale', hostname: HOSTNAME, allowed_capability: 'example.com/cap/open-workspaces', upstream: 'http://127.0.0.1:9/', node_owner_subject: '1001', node_owner_user_id: 1}), {mode: 0o600});
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
await fakeWorker(path.join(data, 'control.sock'), request => verbose && console.log('worker', JSON.stringify(request)));
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

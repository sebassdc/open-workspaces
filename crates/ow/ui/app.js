'use strict';
const $ = id => document.getElementById(id);
let data = {workspaces: [], snapshots: [], stats: {}}, selected = null, busy = false, dialogOperation = null, view = 'machines';
const histories = new Map();
const imageNames = {alpine:'Alpine Linux',arch:'Arch Linux',ubuntu:'Ubuntu 24.04'};
const mib = value => value >= 1024 ? `${(value / 1024).toFixed(1)} GiB` : `${Math.round(value)} MiB`;
function element(tag, text, className) { const node = document.createElement(tag); if (text !== undefined) node.textContent = text; if (className) node.className = className; return node; }
function notice(text, error = false) { $('notice').textContent = text; $('notice').className = `notice${error ? ' error' : ''}`; $('notice').hidden = !text; }
async function api(path, body) {
  const response = await fetch(path, body ? {method: 'POST', headers: {'content-type': 'application/json', 'x-ow-request': 'dashboard'}, body: JSON.stringify(body)} : {});
  const kind = response.headers.get('content-type') || '';
  if (!kind.includes('application/json')) throw Error('Your login may have expired. Reload the page to sign in again.');
  const value = await response.json();
  if (!response.ok || !value.ok) throw Error(value.error || 'The request could not complete. Refresh before retrying.');
  return value.result;
}
async function refresh() { try { data = await api('/api/state'); render(); $('updated').textContent = `Updated ${new Date().toLocaleTimeString([], {hour: '2-digit', minute: '2-digit'})}`; } catch (error) { notice(error.message, true); $('updated').textContent = 'Connection interrupted'; } }
function action(label, callback, disabled = false) { const button = element('button', label, 'button secondary'); button.disabled = disabled || busy; button.addEventListener('click', () => Promise.resolve(callback()).catch(() => {})); return button; }
function select(id) { selected = id; render(); }
function machine() { return data.workspaces.find(item => item.id === selected); }
function setView(next) { view = next; for (const id of ['machines', 'snapshots', 'usage']) $(id + '-view').hidden = id !== view; document.querySelectorAll('.nav').forEach(button => button.classList.toggle('active', button.dataset.view === view)); const names = {machines: ['Your machines.', 'A place to build, experiment, and pick up where you left off.', 'Machines'], snapshots: ['Pick up from here.', 'Saved memory and disk state, ready to restore or fork.', 'Snapshots'], usage: ['Room to grow.', 'See what your machines use, and free capacity when you need it.', 'Resource usage']}; $('page-title').textContent = names[view][0]; $('page-description').textContent = names[view][1]; $('breadcrumb').textContent = 'Your sandbox / ' + names[view][2]; }
function render() {
  if (data.user) { $('account-email').textContent = data.user.email; $('account-avatar').textContent = data.user.email.slice(0,1).toUpperCase(); }
  if (!selected && data.workspaces.length) selected = data.workspaces[0].id;
  if (selected && !machine()) selected = null;
  const running = data.workspaces.filter(item => item.state === 'running').length, stats = data.stats;
  $('metric-running').textContent = running; $('metric-total').textContent = `${data.workspaces.length} machines in your sandbox`;
  $('metric-memory').textContent = mib((stats.total_pss_kib || 0) / 1024);
  $('metric-reserved').textContent = mib(stats.reserved_memory_mib || 0);
  $('metric-limit').textContent = `of ${mib(stats.limit_memory_mib || 4096)} capacity`;
  $('nav-count').textContent = data.workspaces.length; $('machine-total').textContent = data.workspaces.length;
  $('capacity').value = stats.reserved_memory_mib || 0; $('capacity-text').textContent = `${mib(stats.reserved_memory_mib || 0)} / ${mib(stats.limit_memory_mib || 4096)}`;
  $('machine-grid').replaceChildren();
  for (const item of data.workspaces) {
    const card = element('button', undefined, 'machine-card' + (item.id === selected ? ' selected' : '')); card.dataset.machine = item.id; card.setAttribute('aria-pressed', String(item.id === selected));
    const top = element('div', undefined, 'card-top'); top.append(element('span', '▣', 'machine-icon'), element('span', item.state, `state ${['running','hibernated','stopped','failed'].includes(item.state) ? item.state : 'stopped'}`));
    const meta = element('div', undefined, 'card-meta'); meta.append(element('span', `${item.vcpu_count || 1} vCPU`), element('span', mib(item.memory_mib)));
    card.append(top, element('h3', item.id), meta, element('span', item.source ? `Forked from ${item.source}` : (imageNames[item.image || 'alpine'] || 'Linux machine'), 'card-lineage'));
    card.addEventListener('click', () => select(item.id)); $('machine-grid').append(card);
  }
  if (!data.workspaces.length) $('machine-grid').append(element('div', 'Your first machine is one click away. Choose “New machine” to get started.', 'empty'));
  const current = machine(); $('detail').hidden = !current;
  if (current) {
    $('selected-name').textContent = current.id; $('detail-actions').replaceChildren();
    const live = current.state === 'running';
    if (live) { $('detail-actions').append(action('Open Terminal', () => openTerminal(current.id)), action('Snapshot', () => openDialog('snapshot', current.id)), action('Fork', () => openDialog('fork', current.id)), action('Hibernate', () => operate({op: 'hibernate', id: current.id})), action('Stop', () => openDialog('stop', current.id))); }
    else { $('detail-actions').append(action(current.state === 'hibernated' ? 'Resume machine' : 'Start machine', () => operate({op: 'start', id: current.id}))); }
    $('run-command').disabled = !live || busy; $('command').disabled = !live || busy;
    $('console-output').textContent = histories.get(current.id) || (live ? 'Ready. Run a command to get started.\n' : 'This machine is asleep or stopped. Start it to run commands.\n');
  }
  $('snapshot-list').replaceChildren();
  for (const item of data.snapshots.slice().reverse()) {
    const row = element('article', undefined, 'snapshot-row'), info = element('div'); info.append(element('h3', item.name), element('p', `${item.workspace} · ${mib(item.memory_mib)} · memory + disk`));
    const buttons = element('div', undefined, 'actions'); buttons.append(action('Fork a machine', () => openDialog('fork', item.workspace, item.name)), action('Restore', () => openDialog('restore', item.workspace, item.name)));
    row.append(info, buttons); $('snapshot-list').append(row);
  }
  if (!data.snapshots.length) $('snapshot-list').append(element('div', 'No checkpoints yet. Select a running machine and capture a snapshot.', 'empty'));
  $('create').disabled = busy;
}
function field(name, label, value, choices) { const wrapper = element('label', label, 'field'); let input; if (choices) { input = element('select'); for (const [v, title] of choices) { const option = element('option', title); option.value = v; input.append(option); } } else { input = element('input'); input.pattern = '[A-Za-z0-9_-]{1,32}'; input.maxLength = 32; input.required = true; input.autocomplete = 'off'; } input.name = name; if (value) input.value = value; wrapper.append(input); return wrapper; }
function openDialog(op, id, snapshot) {
  if (busy) return; dialogOperation = {op, id, snapshot}; $('dialog-fields').replaceChildren(); $('dialog-error').hidden = true;
  const defaults = {create: ['A fresh place to build.', 'Start a persistent Linux machine. Your disk stays with it when you stop.', 'Create machine'], snapshot: ['Save this moment.', 'Capture this machine’s memory and disk together. You can return to this checkpoint later.', 'Capture snapshot'], fork: ['Try another direction.', 'Create an independent machine from this checkpoint. Changes stay separate from the parent.', 'Create fork'], restore: ['Return to this checkpoint?', 'This replaces the machine’s current memory and disk with the saved snapshot. Changes made since that snapshot will be lost.', 'Restore snapshot'], stop: ['Stop this machine?', 'Disk changes are saved. Running processes and memory state are lost. Choose Hibernate instead to keep them.', 'Stop machine']};
  $('dialog-title').textContent = defaults[op][0]; $('dialog-description').textContent = defaults[op][1]; $('dialog-submit').textContent = defaults[op][2];
  if (op === 'create') $('dialog-fields').append(field('id', 'Machine name', '', null), field('image', 'Operating system', 'alpine', Object.entries(imageNames).map(([value,label]) => [value,value === 'alpine' ? label : `${label} · developer tools`])), field('memory_mib', 'Memory', '256', [['256','256 MiB'],['512','512 MiB'],['1024','1 GiB']]));
  if (op === 'snapshot') $('dialog-fields').append(field('name', 'Snapshot name', `snapshot-${Date.now().toString(36)}`));
  if (op === 'fork') { $('dialog-fields').append(field('child', 'New machine name', '', null)); if (!snapshot) { const choices = [['','Capture a new snapshot now'], ...data.snapshots.filter(item => item.workspace === id).map(item => [item.name,item.name])]; $('dialog-fields').append(field('snapshot', 'Start from', '', choices)); } }
  if (id) $('dialog-fields').append(element('p', `Machine: ${id}${snapshot ? ` · Snapshot: ${snapshot}` : ''}`, 'muted'));
  $('operation-dialog').showModal();
}
async function operate(operation) {
  busy = true; render(); notice(`${operation.op === 'exec' ? 'Running command' : 'Working on your machine'}…`);
  try { const result = await api('/api/operation', operation); if (operation.op === 'fork' || operation.op === 'create') selected = result.id; const messages = {create:'Machine created.', start:'Machine started.', hibernate:'Machine hibernated. Memory and disk state saved.', stop:'Machine stopped. Disk changes retained.', snapshot:'Snapshot saved.', fork:'Independent fork created.', restore:'Snapshot restored.'}; notice(operation.op === 'exec' ? `Command finished · exit ${result.exit_code}` : messages[operation.op], operation.op === 'exec' && result.exit_code !== 0); return result; }
  catch (error) { notice(error.message, true); throw error; }
  finally { busy = false; await refresh(); }
}
$('operation-form').addEventListener('submit', async event => { event.preventDefault(); const fields = Object.fromEntries(new FormData(event.currentTarget)); const op = {...dialogOperation, ...fields}; if (op.memory_mib) op.memory_mib = Number(op.memory_mib); if (op.op === 'restore') op.name = op.snapshot; if (!op.snapshot) delete op.snapshot; if (!op.id) delete op.id; if (op.op === 'restore') delete op.snapshot; $('dialog-submit').disabled = true; $('cancel-dialog').disabled = true; $('close-dialog').disabled = true; try { await operate(op); $('operation-dialog').close(); } catch (error) { $('dialog-error').textContent = error.message; $('dialog-error').hidden = false; } finally { $('dialog-submit').disabled = false; $('cancel-dialog').disabled = false; $('close-dialog').disabled = false; } });
$('console-form').addEventListener('submit', async event => { event.preventDefault(); const current = machine(); if (!current || busy) return; const command = $('command').value.trim(); if (!command) return; let history = histories.get(current.id) || ''; history += `$ ${command}\n`; histories.set(current.id, history); try { const result = await operate({op:'exec',id:current.id,command}); history += result.output + `\n[exit ${result.exit_code}]\n`; histories.set(current.id, history.slice(-200000)); $('command').value = ''; render(); $('console-output').scrollTop = $('console-output').scrollHeight; } catch (error) { histories.set(current.id, history + error.message + '\n'); render(); } });
$('command').addEventListener('keydown', event => { if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); $('console-form').requestSubmit(); } });
$('clear-console').addEventListener('click', () => { if (selected) histories.delete(selected); render(); });
$('create').addEventListener('click', () => openDialog('create'));
$('refresh').addEventListener('click', refresh);
for (const id of ['close-dialog','cancel-dialog']) $(id).addEventListener('click', () => { if (!busy) $('operation-dialog').close(); });
$('operation-dialog').addEventListener('cancel', event => { if (busy) event.preventDefault(); });
document.querySelectorAll('.nav').forEach(button => button.addEventListener('click', () => setView(button.dataset.view)));
refresh(); setInterval(() => { if (!busy && !document.hidden && !$('operation-dialog').open) refresh(); }, 5000);

let terminalSocket = null, terminalView = null, terminalFit = null, terminalObserver = null;
function closeTerminal() {
  if (terminalSocket) { terminalSocket.close(); terminalSocket = null; }
  if (terminalObserver) { terminalObserver.disconnect(); terminalObserver = null; }
  if (terminalView) { terminalView.dispose(); terminalView = null; }
  terminalFit = null; $('terminal-screen').replaceChildren();
  if ($('terminal-dialog').open) $('terminal-dialog').close();
}
function openTerminal(id) {
  closeTerminal(); $('terminal-title').textContent = `${id} / Terminal`; $('terminal-status').textContent = 'Connecting…';
  $('terminal-dialog').showModal();
  const terminal = new Terminal({cursorBlink:true, fontSize:14, scrollback:2000, theme:{background:'#101c18',foreground:'#e7f1eb'}});
  const fit = new FitAddon.FitAddon(); terminal.loadAddon(fit); terminal.open($('terminal-screen')); fit.fit();
  terminalView = terminal; terminalFit = fit;
  const socket = new WebSocket(`${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/api/terminal/${encodeURIComponent(id)}`);
  socket.binaryType = 'arraybuffer'; terminalSocket = socket;
  const resize = () => { fit.fit(); if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify({type:'resize',cols:terminal.cols,rows:terminal.rows})); };
  terminalObserver = new ResizeObserver(resize); terminalObserver.observe($('terminal-screen'));
  socket.onopen = () => { if (terminalSocket !== socket) { socket.close(); return; } $('terminal-status').textContent = 'Connected'; resize(); terminal.focus(); };
  socket.onmessage = event => {
    if (terminalSocket !== socket) return;
    if (typeof event.data === 'string') {
      const control = JSON.parse(event.data); if (control.type === 'exit') $('terminal-status').textContent = `Shell exited · ${control.code}`;
    } else {
      const bytes = new Uint8Array(event.data); terminal.write(bytes, () => { if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify({type:'ack',bytes:bytes.length})); });
    }
  };
  socket.onerror = () => { if (terminalSocket === socket) $('terminal-status').textContent = 'Connection failed. Check the machine and reload to sign in.'; };
  socket.onclose = () => { if (terminalSocket === socket && $('terminal-status').textContent === 'Connected') $('terminal-status').textContent = 'Disconnected. Reopen Terminal to reconnect.'; };
  terminal.onData(text => {
    if (socket.readyState !== WebSocket.OPEN) return;
    const bytes = new TextEncoder().encode(text);
    if (socket.bufferedAmount + bytes.length > 65536) { socket.close(); $('terminal-status').textContent = 'Input exceeded connection capacity. Reconnect to continue.'; return; }
    for (let offset=0; offset<bytes.length; offset+=4096) socket.send(bytes.slice(offset,offset+4096));
  });
}
$('terminal-close').addEventListener('click', closeTerminal);
$('terminal-dialog').addEventListener('cancel', event => { event.preventDefault(); closeTerminal(); });
window.addEventListener('pagehide', closeTerminal);

const installCommand = `curl -fsSL ${location.origin}/cli/install.sh | sh`;
$('cli-command').textContent = installCommand;
$('cli-login').textContent = `ow login ${location.origin}`;
$('copy-install').addEventListener('click', async () => {
  try { await navigator.clipboard.writeText(installCommand); $('copy-install').textContent = 'Copied'; setTimeout(() => { $('copy-install').textContent = 'Copy command'; }, 2000); }
  catch { notice('Select the install command and copy it to your terminal.'); }
});

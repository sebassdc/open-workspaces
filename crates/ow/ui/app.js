'use strict';
const $ = id => document.getElementById(id);
let data = {workspaces: [], snapshots: [], stats: {}}, loaded = false, selected = null, busy = false, dialogOperation = null, view = 'machines';
const histories = new Map();
const imageNames = {alpine:'Alpine Linux',arch:'Arch Linux',ubuntu:'Ubuntu 24.04', 'ubuntu-arm64':'Ubuntu 24.04 ARM64'};
// Built-in images, then golden images that the runners advertise (`ow image build`).
const imageChoices = () => [...Object.entries(imageNames).map(([value,label]) => [value,value === 'alpine' ? label : `${label} · developer tools`]),
  ...[...new Set((data.nodes || []).flatMap(n => n.capabilities?.images || []))].filter(i => !Object.hasOwn(imageNames, i)).sort().map(i => [i, `${i} · golden image`])];
const diskUsage = item => item.disk_bytes ? `${mib(item.disk_bytes / 1048576)} disk · ${item.disk_used_bytes == null ? 'use unknown' : mib(item.disk_used_bytes / 1048576) + ' used'}` : 'Disk size unavailable';
const mib = value => value >= 1024 ? `${(value / 1024).toFixed(1)} GiB` : `${Math.round(value)} MiB`;
function element(tag, text, className) { const node = document.createElement(tag); if (text !== undefined) node.textContent = text; if (className) node.className = className; return node; }
function notice(text, error = false) { $('notice').textContent = text; $('notice').className = `notice${error ? ' error' : ''}`; $('notice').hidden = !text; }
async function api(path, body) {
  const response = await fetch(path, body ? {method: 'POST', headers: {'content-type': 'application/json', 'x-ow-request': 'dashboard'}, body: JSON.stringify(body)} : {});
  const kind = response.headers.get('content-type') || '';
  if (!kind.includes('application/json')) throw Error('Your login may have expired. Reload the page to sign in again.');
  const value = await response.json();
  if (!response.ok || !value.ok) { const error = Error((value.error || 'The request could not complete. Refresh before retrying.') + (value.operation ? ` Operation ${value.operation.id}: ${value.operation.state}; retry key ${value.operation.retry_key}.` : '')); error.operation = value.operation; throw error; }
  return value.result;
}
async function refresh() { try { data = await api('/api/state'); $('host-controls').hidden = !data.host_controls; loaded = true; render(); $('updated').textContent = `Updated ${new Date().toLocaleTimeString([], {hour: '2-digit', minute: '2-digit'})}`; } catch (error) { notice(error.message, true); $('updated').textContent = 'Connection interrupted'; if (!loaded) loadFailed(); } }
// Without a first state the grid has nothing to show, so it offers a retry instead.
function loadFailed() { const retry = element('button', 'Retry', 'text-button'); retry.addEventListener('click', refresh); const empty = element('div', 'Could not reach your machines. ', 'empty load-error'); empty.append(retry); $('machine-grid').replaceChildren(empty); }
function action(label, callback, disabled = false) { const button = element('button', label, 'button secondary'); button.disabled = disabled || busy; button.addEventListener('click', () => Promise.resolve(callback()).catch(() => {})); return button; }
function select(id) { selected = id; render(); if (matchMedia('(max-width:700px)').matches) document.querySelector('.detail-heading').scrollIntoView({block: 'start'}); }
function durability(item) {
  const d = item.durability;
  if (!d) return 'Off-host recovery status unavailable';
  const saved = d.last_durable_at ? `Last durable ${new Date(d.last_durable_at * 1000).toLocaleString()}` : 'No durable recovery point yet';
  const retry = d.retry_at ? ` · retry ${new Date(d.retry_at * 1000).toLocaleTimeString()}` : '';
  return `${saved}${(!d.current_state_durable && d.last_durable_at && Date.now() / 1000 - d.last_durable_at > 3600) ? ' · one-hour disk recovery target exceeded' : ''}${d.pending ? ' · upload pending' : ''}${d.error ? ` · ${d.error}${retry}` : ''}${d.last_durable_at ? ' · off-host copies retained' : ''}`;
}
function machine() { return [...data.workspaces, ...(data.shared || [])].find(item => item.id === selected); }
function setView(next) { view = next; for (const id of ['machines', 'snapshots', 'usage']) $(id + '-view').hidden = id !== view; document.querySelectorAll('.nav').forEach(button => button.classList.toggle('active', button.dataset.view === view)); const names = {machines: ['Your machines.', 'A place to build, experiment, and pick up where you left off.', 'Machines'], snapshots: ['Pick up from here.', 'Saved memory and disk state, ready to restore or fork.', 'Snapshots'], usage: ['Room to grow.', 'See what your machines use, and free capacity when you need it.', 'Resource usage']}; $('page-title').textContent = names[view][0]; $('page-description').textContent = names[view][1]; $('breadcrumb').textContent = 'Your sandbox / ' + names[view][2]; }
function render() {
  if (data.user) { $('account-email').textContent = data.user.email; $('account-avatar').textContent = data.user.email.slice(0,1).toUpperCase(); }
  if (!selected && data.workspaces.length) selected = data.workspaces[0].id;
  if (selected && !machine()) selected = null;
  const running = data.workspaces.filter(item => item.state === 'running').length, stats = data.stats;
  const capacity = stats.limit_memory_mib ?? 0;
  $('metric-running').textContent = running; $('metric-total').textContent = `${data.workspaces.length} machines in your sandbox`;
  $('metric-memory').textContent = mib((stats.total_pss_kib || 0) / 1024);
  // Owner caps refuse a create; pool-wide totals are shared and change as hosts join.
  const {caps = {}, usage = {}} = data.owner_limits || {};
  $('metric-reserved').textContent = `${mib(usage.memory_mib || 0)} of ${mib(caps.memory_mib || 0)}`;
  $('metric-limit').textContent = `${usage.boxes || 0} of ${caps.boxes || 0} machines · ${usage.cpus || 0} of ${caps.cpus || 0} vCPU · ${usage.disk_gib || 0} of ${caps.disk_gib || 0} GiB disk`;
  $('nav-count').textContent = data.workspaces.length; $('machine-total').textContent = data.workspaces.length;
  $('capacity').hidden = capacity === 0; $('capacity').max = capacity || 1; $('capacity').value = stats.reserved_memory_mib || 0; $('capacity-text').textContent = `${mib(stats.reserved_memory_mib || 0)} / ${mib(capacity)}`;
  $('machine-grid').replaceChildren(); $('shared-grid').replaceChildren(); $('shared-view').hidden = !(data.shared || []).length;
  for (const item of [...data.workspaces, ...(data.shared || [])]) {
    const card = element('button', undefined, 'machine-card' + (item.id === selected ? ' selected' : '')); card.dataset.machine = item.id; card.setAttribute('aria-pressed', String(item.id === selected));
    const top = element('div', undefined, 'card-top'); top.append(element('span', '▣', 'machine-icon'), element('span', item.state, `state ${['running','hibernated','stopped','failed'].includes(item.state) ? item.state : 'stopped'}`));
    const meta = element('div', undefined, 'card-meta'); meta.append(element('span', `${item.vcpu_count || 1} vCPU`), element('span', mib(item.memory_mib)));
    card.append(top, element('h3', item.id), meta, element('span', diskUsage(item), 'card-lineage'), element('span', item.source ? `Forked from ${item.source}` : (imageNames[item.image || 'alpine'] || 'Linux machine'), 'card-lineage')); if (item.node) card.append(element('span', `${item.node}${item.node_online === false ? ' · offline' : ''}`, 'card-lineage'));
    if (item.owner) card.append(element('span', `Shared by ${item.owner} · ${item.role}`, 'card-lineage'));
    const recovery = element('span', durability(item), 'card-lineage' + (item.durability && (!item.durability.last_durable_at || item.durability.error || (!item.durability.current_state_durable && Date.now() / 1000 - item.durability.last_durable_at > 3600)) ? ' recovery-warning' : ''));
    if (item.durability) recovery.title = item.durability.retention || '';
    card.append(recovery);
    card.addEventListener('click', () => select(item.id)); $(item.owner ? 'shared-grid' : 'machine-grid').append(card);
  }
  if (!data.workspaces.length) $('machine-grid').append(element('div', 'Your first machine is one click away. Choose “New machine” to get started.', 'empty'));
  const current = machine(); $('detail').hidden = !current;
  if (current) {
    $('selected-name').textContent = current.id; $('detail-actions').replaceChildren();
    const live = current.state === 'running';
    const operations=(data.nodes||[]).find(n=>n.id===current.node)?.capabilities?.operations;
    const supports=op=>!operations || operations.includes(op);
    const owned = !current.role, admin = owned || current.role === 'admin';
    if (live) { $('detail-actions').append(action('Open Terminal', () => openTerminal(current.id))); if (admin) $('detail-actions').append(action('Snapshot', () => openDialog('snapshot', current.id), !supports('snapshot'))); $('detail-actions').append(action('Fork', () => openDialog('fork', current.id), !supports('fork')), action('Hibernate', () => operate({op: 'hibernate', id: current.id}), !supports('hibernate'))); if (admin) $('detail-actions').append(action('Stop', () => openDialog('stop', current.id))); }
    else if (current.state !== 'missing' && !current.unmade) { $('detail-actions').append(action(current.state === 'hibernated' ? 'Resume machine' : 'Start machine', () => operate({op: 'start', id: current.id}))); }
    if (owned && current.state === 'stopped') $('detail-actions').append(action('Resize', () => openDialog('resize', current.id), !supports('resize')));
    if (owned && data.previews) $('detail-actions').append(action('Publish port', () => openDialog('publish', current.id), !supports('tunnel')));
    if (owned) $('detail-actions').append(action('Rename', () => openDialog('rename', current.id)), action('Share', () => openDialog('share', current.id)));
    if (owned && !live) $('detail-actions').append(action('Delete', () => openDialog('delete', current.id), !supports('delete')));
    $('published-ports').replaceChildren();
    for (const port of current.published_ports || []) {
      const row = element('span', `Port ${port.port}`, 'actions'), link = element('a', 'Open preview', 'button secondary');
      link.href = port.url; link.target = '_blank'; link.rel = 'noopener noreferrer'; row.append(link);
      if (owned) row.append(action('Unpublish', () => operate({op:'unpublish',id:current.id,port:port.port})));
      $('published-ports').append(row);
    }
    $('shared-with').replaceChildren(); $('shared-with').hidden = !(current.shared_with || []).length;
    for (const grant of current.shared_with || []) { const row = element('span', `Shared with ${grant.email} · ${grant.role} `); row.append(action('Unshare', () => operate({op: 'unshare', id: current.id, with: grant.email}))); $('shared-with').append(row); }
    $('run-command').disabled = !live || busy; $('command').disabled = !live || busy;
    $('console-output').textContent = histories.get(current.id) || (live ? 'Ready. Run a command to get started.\n' : 'This machine is asleep or stopped. Start it to run commands.\n');
  }
  $('snapshot-list').replaceChildren();
  for (const item of data.snapshots.slice().reverse()) {
    const row = element('article', undefined, 'snapshot-row'), info = element('div'); info.append(element('h3', item.name), element('p', item.state === 'ready' ? `${item.workspace} · ${mib(item.memory_mib)} · memory + disk` : item.state === 'failed' ? 'Capture failed. Retry from the running machine.' : `Capture ${item.state || 'pending'}. Check operation status before retrying.`));
    const buttons = element('div', undefined, 'actions'); if (item.state === 'ready') buttons.append(action('Fork a machine', () => openDialog('fork', item.workspace, item.name)), action('Restore', () => openDialog('restore', item.workspace, item.name)));
    row.append(info, buttons); $('snapshot-list').append(row);
  }
  if (!data.snapshots.length) $('snapshot-list').append(element('div', 'No checkpoints yet. Select a running machine and capture a snapshot.', 'empty'));
  $('host-node-list').replaceChildren();
  if(data.host_controls) for(const node of (data.nodes||[]).filter(n=>!n.revoked)) {
    const row=element('div'); row.append(element('span',`${node.id} · ${node.online?'online':'offline'} · ${mib(node.memory_mib)} `),action('Adjust budget',()=>hostDialog('budget',node.id))); $('host-node-list').append(row);
  }
  $('create').disabled = busy || !loaded;
}
function field(name, label, value, choices) { const wrapper = element('label', label, 'field'); let input; if (choices) { input = element('select'); for (const [v, title] of choices) { const option = element('option', title); option.value = v; input.append(option); } } else { input = element('input'); input.pattern = '[A-Za-z0-9_-]{1,32}'; input.maxLength = 32; input.required = true; input.autocomplete = 'off'; } input.name = name; if (value) input.value = value; wrapper.append(input); return wrapper; }
function openDialog(op, id, snapshot) {
  if (busy) return; dialogOperation = {op, id, snapshot}; $('dialog-fields').replaceChildren(); $('dialog-error').hidden = true;
  const defaults = {publish: ['Publish a port.', 'Open a guest HTTP server over tailnet HTTPS. Only you and teammates with a share can access it. The server must listen on 0.0.0.0; preview cookies are disabled.', 'Publish port'], resize: ['Grow this machine.', 'The machine must be stopped. Disk growth is permanent; its filesystem expands on the next start.', 'Resize machine'], create: ['A fresh place to build.', 'Start a persistent Linux machine. Your disk stays with it when you stop.', 'Create machine'], snapshot: ['Save this moment.', 'Capture this machine’s memory and disk together. You can return to this checkpoint later.', 'Capture snapshot'], fork: ['Try another direction.', 'Create an independent machine from this checkpoint. Changes stay separate from the parent.', 'Create fork'], restore: ['Return to this checkpoint?', 'This replaces the machine’s current memory and disk with the saved snapshot. Changes made since that snapshot will be lost.', 'Restore snapshot'], stop: ['Stop this machine?', 'Disk changes are saved. Running processes and memory state are lost. Choose Hibernate instead to keep them.', 'Stop machine'], share: ['Share this machine.', 'Your teammate gets in with their own login. A fork they make copies everything on this machine, including your credentials, and stays theirs after you unshare.', 'Share machine'], rename: ['Rename this machine.', 'Only the name changes: disk, snapshots, shares, previews and running state stay. SSH config and teammates use the new name from now on.', 'Rename machine'], delete: ['Delete this machine?', 'Its disk and every snapshot taken from it are removed for good, and anyone it is shared with loses it. Forks made earlier stay.', 'Delete machine']};
  const destructive = op === 'delete' || op === 'restore'; $('dialog-eyebrow').textContent = destructive ? 'CANNOT BE UNDONE' : 'YOUR SANDBOX'; $('dialog-eyebrow').classList.toggle('warning', destructive); $('dialog-submit').className = `button ${destructive ? 'danger' : 'primary'}`;
  $('dialog-title').textContent = defaults[op][0]; $('dialog-description').textContent = defaults[op][1]; $('dialog-submit').textContent = defaults[op][2];
  if (op === 'create') $('dialog-fields').append(field('id', 'Machine name', '', null), field('image', 'Operating system', 'alpine', imageChoices()), field('memory_mib', 'Memory', '256', [['256','256 MiB'],['512','512 MiB'],['1024','1 GiB'],['2048','2 GiB'],['4096','4 GiB'],['8192','8 GiB'],['16384','16 GiB']]), field('vcpu_count', 'vCPU', '1', Array.from({length:16},(_,i)=>[String(i+1),String(i+1)])), field('node', 'Pool node', '', [['','Automatic placement'], ...(data.nodes || []).filter(n => n.online).map(n => [n.id, n.id])]));
  if (op === 'create' || op === 'resize') {
    const size = op === 'resize' ? Math.ceil((machine()?.disk_bytes || 0) / 1073741824) : 32;
    const disk = field('disk_gib', 'Root disk (GiB)', String(size || 32));
    const input = disk.querySelector('input'); input.type = 'number'; input.removeAttribute('pattern'); input.removeAttribute('maxlength'); input.min = op === 'create' ? '1' : String(Math.max(1, size)); input.max = '16384'; input.step = '1';
    $('dialog-fields').append(disk);
  }
  if (op === 'create') {
    const form=$('operation-form'), image=form.elements.image, memory=form.elements.memory_mib, node=form.elements.node;
    const fitShape=()=>{const mac=image.value==='ubuntu-arm64'; if(mac && Number(memory.value)<512) memory.value='512'; form.elements.disk_gib.disabled=mac; form.elements.disk_gib.parentElement.hidden=mac;};
    image.addEventListener('change',fitShape);
    node.addEventListener('change',()=>{const selectedNode=(data.nodes||[]).find(n=>n.id===node.value);const images=selectedNode?.capabilities?.images;if(images?.length===1) image.value=images[0];fitShape();});
  }
  if (op === 'publish') { const port = field('port', 'Guest HTTP port', '3000'), input = port.querySelector('input'); input.type = 'number'; input.removeAttribute('pattern'); input.removeAttribute('maxlength'); input.min = '1'; input.max = '65535'; input.step = '1'; $('dialog-fields').append(port); }
  if (op === 'share') { const email = field('with', 'Teammate email', '', null), input = email.querySelector('input'); input.type = 'email'; input.removeAttribute('pattern'); input.maxLength = 254; $('dialog-fields').append(email, field('role', 'Role', 'use', [['use','Use · terminal, SSH, commands, fork'],['admin','Admin · also stop, snapshot, restore, SSH keys']])); }
  if (op === 'rename') $('dialog-fields').append(field('name', 'New machine name', id, null));
  if (op === 'delete') { const going = data.snapshots.filter(item => item.workspace === id).map(item => item.name); $('dialog-fields').append(element('p', going.length ? `Its ${going.length} snapshot${going.length === 1 ? '' : 's'} will be deleted too: ${going.join(', ')}` : 'It has no snapshots.', going.length ? 'field-error' : 'muted')); }
  if (op === 'snapshot') $('dialog-fields').append(field('name', 'Snapshot name', `snapshot-${Date.now().toString(36)}`));
  if (op === 'fork') { $('dialog-fields').append(field('child', 'New machine name', '', null)); if (!snapshot) { const choices = [['','Capture a new snapshot now'], ...data.snapshots.filter(item => item.workspace === id).map(item => [item.name,item.name])]; $('dialog-fields').append(field('snapshot', 'Start from', '', choices)); } }
  if (id) $('dialog-fields').append(element('p', `Machine: ${id}${snapshot ? ` · Snapshot: ${snapshot}` : ''}`, 'muted'));
  $('operation-dialog').showModal();
}
async function operate(operation) {
  const {operation_key: suppliedKey, ...parameters} = operation; const intent = JSON.stringify(parameters); const storageKey = 'ow-operation:' + intent; operation.operation_key ||= sessionStorage.getItem(storageKey) || crypto.randomUUID().replaceAll('-',''); sessionStorage.setItem(storageKey, operation.operation_key);
  busy = true; render(); notice(`${operation.op === 'exec' ? 'Running command' : 'Working on your machine'}…`);
  try { const result = await api('/api/operation', operation); if (operation.op === 'fork' || operation.op === 'create') selected = result.id; if (operation.op === 'rename' && result.id !== operation.id) { if (selected === operation.id) selected = result.id; if (histories.has(operation.id)) { histories.set(result.id, histories.get(operation.id)); histories.delete(operation.id); } } sessionStorage.removeItem(storageKey); const messages = {publish:'Port published. Open the preview below.', unpublish:'Port unpublished; preview connections closed.', resize:'Machine resized. Start it to grow the filesystem.', create:'Machine created.', start:'Machine started.', hibernate:'Machine hibernated. Memory and disk state saved.', stop:'Machine stopped. Disk changes retained.', snapshot:'Snapshot saved.', fork:'Independent fork created.', restore:'Snapshot restored.', share:'Machine shared.', rename:'Machine renamed.', delete:'Machine deleted.', unshare:'Access removed; their open sessions were closed.'}; notice(operation.op === 'exec' ? `Command finished · exit ${result.exit_code}` : messages[operation.op], operation.op === 'exec' && result.exit_code !== 0); return result; }
  catch (error) { if (error.operation?.state === 'failed') sessionStorage.removeItem(storageKey); notice(error.message, true); throw error; }
  finally { busy = false; await refresh(); }
}
$('operation-form').addEventListener('submit', async event => { event.preventDefault(); const fields = Object.fromEntries(new FormData(event.currentTarget)); const op = {...dialogOperation, ...fields}; if (op.port) op.port = Number(op.port); if (op.disk_gib) op.disk_gib = Number(op.disk_gib); if (op.memory_mib) op.memory_mib = Number(op.memory_mib); if (op.vcpu_count) op.vcpu_count = Number(op.vcpu_count); if (op.op === 'resize' && op.disk_gib === Math.ceil((machine()?.disk_bytes || 0) / 1073741824)) { $('dialog-error').textContent = `Already ${op.disk_gib} GiB. Choose a larger size to grow it.`; $('dialog-error').hidden = false; return; } if (op.op === 'restore') op.name = op.snapshot; if (!op.node) delete op.node; if (!op.snapshot) delete op.snapshot; if (!op.id) delete op.id; if (op.op === 'restore') delete op.snapshot; $('dialog-submit').disabled = true; $('cancel-dialog').disabled = true; $('close-dialog').disabled = true; try { await operate(op); $('operation-dialog').close(); } catch (error) { $('dialog-error').textContent = error.message; $('dialog-error').hidden = false; } finally { $('dialog-submit').disabled = false; $('cancel-dialog').disabled = false; $('close-dialog').disabled = false; } });
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
  const socket = new WebSocket(`${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/api/terminal/${id}`); // IDs are validated URL-safe; owner@example.com/name keeps its slash
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
// Tailnet sign-in serves no CLI installer: the CLI signs in through Access only.
fetch('/cli/install.sh', {cache: 'no-store'}).then(r => { if (!r.ok) $('cli-install').hidden = true; }).catch(() => { $('cli-install').hidden = true; });
$('copy-install').addEventListener('click', async () => {
  try { await navigator.clipboard.writeText(installCommand); $('copy-install').textContent = 'Copied'; setTimeout(() => { $('copy-install').textContent = 'Copy command'; }, 2000); }
  catch { notice('Select the install command and copy it to your terminal.'); }
});

let hostMode='invite';
function hostDialog(mode,node='') { hostMode=mode; $('host-description').textContent=mode==='budget'?'Set owner ceilings for this enrolled host. Its operator must separately consent to a stopped-host configuration. Lowering requires online idle inventory with no unresolved demand.':`Share this one-use invitation privately. ${data.pool_policy || 'Host participation grants no human dashboard login.'}`; $('host-title').textContent=mode==='budget'?'Adjust enrolled host budget':'Invite a trusted Linux host'; $('host-form').elements.ttl.parentElement.hidden=mode==='budget'; $('host-submit').textContent=mode==='budget'?'Save owner limits':'Create invitation'; $('host-form').reset(); $('host-envelope').value=''; $('host-envelope').hidden=true; $('host-instructions').hidden=true; $('host-submit').disabled=false; $('host-error').textContent=''; $('host-form').elements.node.value=node; const limits=(data.nodes||[]).find(n=>n.id===node)?.owner_limits; if(limits) for(const key of ['memory','slots','cpus']) $('host-form').elements[key].value=limits[key]; $('host-dialog').showModal(); }
$('host-invite').addEventListener('click',()=>hostDialog('invite'));
$('host-budget').addEventListener('click',()=>hostDialog('budget'));
function clearHostInvitation() { $('host-envelope').value=''; $('host-envelope').hidden=true; $('host-instructions').hidden=true; }
$('host-close').addEventListener('click', () => { clearHostInvitation(); $('host-dialog').close(); });
$('host-dialog').addEventListener('close', clearHostInvitation);
$('host-form').addEventListener('submit', async event => { event.preventDefault(); $('host-submit').disabled=true; $('host-error').textContent=''; const fields=new FormData(event.target); const body={node:fields.get('node')}; for(const key of (hostMode==='invite'?['memory','slots','cpus','ttl']:['memory','slots','cpus'])) body[key]=Number(fields.get(key)); try {const invite=await api('/api/hosts/'+hostMode,body); if(hostMode==='budget') { $('host-dialog').close(); notice('Owner limits saved. Ask the operator to configure the stopped host with explicit consent.'); await refresh(); return; } $('host-envelope').value=JSON.stringify(invite); $('host-envelope').hidden=false; $('host-instructions').hidden=false;} catch(error) {$('host-error').textContent=error.message+(hostMode==='invite'?' If the response was lost, creating another invitation for this unused node invalidates the first.':' Inspect pool state before retrying.'); $('host-submit').disabled=false;} });
$('host-revoke').addEventListener('click', async () => {const node=window.prompt('Node label to revoke. Already accepted effects cannot be undone; the host operator must stop running guests.'); if(!node)return;try{await api('/api/hosts/revoke',{node});notice('Node revoked. Ask its operator to stop participation.');await refresh();}catch(error){notice(error.message,true);}});

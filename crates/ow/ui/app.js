'use strict';
const $ = id => document.getElementById(id);
let data = {workspaces: [], snapshots: [], stats: {}}, loaded = false, selected = null, busy = false, dialogOperation = null, view = 'machines';
const histories = new Map();
const imageNames = {alpine:'Alpine Linux',arch:'Arch Linux',ubuntu:'Ubuntu 24.04', 'ubuntu-arm64':'Ubuntu 24.04 ARM64'};
// Built-in images, then golden images that the runners advertise (`ow image build`).
// Discovery decides availability once the pool has nodes; a lone local host has none and decides itself.
const imageChoices = () => { const pool = data.discovery?.nodes?.length ? data.discovery.images || [] : null;
  return [...Object.entries(imageNames).map(([value,label]) => [value,value === 'alpine' ? label : `${label} · developer tools`]),
  ...[...new Set((data.nodes || []).flatMap(n => n.capabilities?.images || []))].filter(i => !Object.hasOwn(imageNames, i)).sort().map(i => [i, `${i} · golden image`])]
  .map(([value,label]) => pool && !pool.some(i => i.name === value && i.available) ? [value, `${imageNames[value] || value} · not available on this pool`, true] : [value,label]); };
// Online pool nodes with their free room; a full or draining node stays listed but cannot be picked.
const nodeChoices = () => (data.discovery?.nodes || []).filter(n => n.online).map(n => { const free = n.available, full = free && (!free.slots || !free.vcpus || free.memory_mib < 256 || n.workspaces?.available === 0);
  return n.draining ? [n.id, `${n.id} · draining`, true] : full ? [n.id, `${n.id} · full`, true] : [n.id, free ? `${n.id} · ${free.vcpus} vCPU · ${mib(free.memory_mib)} free` : n.id]; });
// Refusals the dashboard can explain in its own words; other errors keep the server's text.
const refusals = {name_taken: 'You already have a machine with that name. Choose another name.', unavailable_image: 'That operating system is not available on this pool right now. Choose another one.', insufficient_capacity: 'The pool has no room for a machine this size right now. Choose less memory or fewer vCPUs, another node, or try again later.', unsupported_capability: 'This pool cannot run a machine of that shape. Choose another operating system or size.', node_unavailable: 'No pool node is online right now. Try again in a moment.'};
const diskUsage = item => item.disk_bytes ? `${mib(item.disk_bytes / 1048576)} disk · ${item.disk_used_bytes == null ? 'use unknown' : mib(item.disk_used_bytes / 1048576) + ' used'}` : 'Disk size unavailable';
const mib = value => value >= 1024 ? `${(value / 1024).toFixed(1)} GiB` : `${Math.round(value)} MiB`;
function element(tag, text, className) { const node = document.createElement(tag); if (text !== undefined) node.textContent = text; if (className) node.className = className; return node; }
// Refusals the server already words for people; shown without operation or retry-key detail.
const plainRefusals = new Set(['fork_capacity', 'already_owner']);
function notice(text, error = false) { $('notice').textContent = text; $('notice').className = `notice${error ? ' error' : ''}`; $('notice').hidden = !text; }
async function api(path, body) {
  const response = await fetch(path, body ? {method: 'POST', headers: {'content-type': 'application/json', 'x-ow-request': 'dashboard'}, body: JSON.stringify(body)} : {});
  const kind = response.headers.get('content-type') || '';
  if (response.status === 401 || response.status === 403 || (response.redirected && response.ok && !kind.includes('application/json'))) throw Error('Your login may have expired. Reload the page to sign in again.');
  if (!kind.includes('application/json')) throw Error(response.status >= 500 ? `The server is not answering (HTTP ${response.status}); it may be busy or restarting. ${body ? 'Wait a moment, then try again.' : 'The page retries on its own.'}` : `Unexpected response from the server (HTTP ${response.status}). Reload the page.`);
  const value = await response.json();
  if (!response.ok || !value.ok) { const code = value.error_code || value.operation?.error_code; const plain = refusals[code] || (plainRefusals.has(code) ? value.error : null); const error = Error(plain || (value.error || 'The request could not complete. Refresh before retrying.') + (value.operation ? ` Operation ${value.operation.id}: ${value.operation.state}; retry key ${value.operation.retry_key}.` : '')); error.operation = value.operation; throw error; }
  return value.result;
}
let refreshError = null;
async function refresh() { try { data = await api('/api/state'); if (refreshError && $('notice').textContent === refreshError) notice(''); refreshError = null; $('host-controls').hidden = !data.host_controls; loaded = true; render(); checkTerminal(); $('updated').textContent = `Updated ${new Date().toLocaleTimeString([], {hour: '2-digit', minute: '2-digit'})}`; } catch (error) { refreshError = error.message; notice(error.message, true); $('updated').textContent = 'Connection interrupted'; if (!loaded) loadFailed(); } }
// Without a first state the grid has nothing to show, so it offers a retry instead.
function loadFailed() { const retry = element('button', 'Retry', 'text-button'); retry.addEventListener('click', refresh); const empty = element('div', 'Could not reach your machines. ', 'empty load-error'); empty.append(retry); $('machine-grid').replaceChildren(empty); }
function action(label, callback, disabled = false) { const button = element('button', label, 'button secondary'); button.disabled = disabled || busy; button.addEventListener('click', () => Promise.resolve(callback()).catch(() => {})); return button; }
// A result belongs to the view and machine it came from; drop it when the user moves on.
function select(id) { if (id !== selected) notice(''); selected = id; render(); if (matchMedia('(max-width:700px)').matches) document.querySelector('.detail-heading').scrollIntoView({block: 'start'}); }
const ago = at => { const s = Date.now() / 1000 - at; return s < 60 ? 'just now' : s < 3600 ? `${Math.floor(s / 60)} min ago` : s < 86400 ? `${Math.floor(s / 3600)} h ago` : `${Math.floor(s / 86400)} d ago`; };
const fullTime = at => new Date(at * 1000).toLocaleString();
// [text, warning, title]: warn only once the one-hour target is really missed or an upload failed.
function durability(item) {
  const d = item.durability, now = Date.now() / 1000, young = item.created_at != null && now - item.created_at < 3600;
  if (!d) return [young ? 'First recovery point pending' : 'Off-host recovery status unavailable', false, ''];
  const last = d.last_durable_at, exceeded = !d.current_state_durable && (last ? now - last > 3600 : !young);
  const saved = last ? `Last durable ${ago(last)}` : young ? 'First recovery point pending' : 'No durable recovery point yet';
  const retry = d.retry_at ? ` · retry ${new Date(d.retry_at * 1000).toLocaleTimeString()}` : '';
  return [`${saved}${exceeded ? ' · one-hour disk recovery target exceeded' : ''}${d.pending && (last || !young) ? ' · upload pending' : ''}${d.error ? ` · ${d.error}${retry}` : ''}${last ? ' · off-host copies retained' : ''}`,
    Boolean(d.error) || exceeded, [last ? `Last durable ${fullTime(last)}` : '', d.retention || ''].filter(Boolean).join('\n')];
}
function snapshotStatus(item) {
  // A live fork's own capture: named by the worker, so say which fork it was for.
  const fork = item.fork ? `Taken for fork ${item.fork} · ` : /^fork-[0-9a-f]{24}$/.test(item.name) ? 'Taken for a fork · ' : '';
  if (item.state === 'ready') return element('p', `${fork}${item.workspace} · ${mib(item.memory_mib)} · memory + disk`);
  if (item.state !== 'failed') return element('p', `Capture ${item.state || 'pending'}. Check operation status before retrying.`);
  const when = item.failed_at ? ` ${ago(item.failed_at)}` : '', line = element('p', item.workspace ? `Capture of ${item.workspace} failed${when}. Retry from the running machine.` : `Capture failed${when} · machine deleted.`, 'recovery-warning');
  if (item.failed_at) line.title = fullTime(item.failed_at);
  return line;
}
function machine() { return [...data.workspaces, ...(data.shared || []), ...(data.pool || [])].find(item => item.id === selected); }
function setView(next) { if (next !== view) notice(''); view = next; for (const id of ['machines', 'snapshots', 'usage']) $(id + '-view').hidden = id !== view; document.querySelectorAll('.nav').forEach(button => button.classList.toggle('active', button.dataset.view === view)); const names = {machines: ['Your machines.', 'A place to build, experiment, and pick up where you left off.', 'Machines'], snapshots: ['Pick up from here.', 'Saved memory and disk state, ready to restore or fork.', 'Snapshots'], usage: ['Room to grow.', 'See what your machines use, and free capacity when you need it.', 'Resource usage']}; $('page-title').textContent = names[view][0]; $('page-description').textContent = names[view][1]; $('breadcrumb').textContent = 'Your sandbox / ' + names[view][2]; }
// Cards have changing status text; use their machine id alone, and row identity plus label for actions.
const controlKey = node => node.dataset.machine ?? `${node.closest('[data-key]')?.dataset.key || ''}\n${node.textContent}`;
function focusKey() { const node = document.activeElement, box = node?.parentElement?.closest('[id]'); return box && node.matches('button, a') ? [box.id, controlKey(node)] : null; }
function restoreFocus(key) { if (!key || (document.activeElement && document.activeElement !== document.body)) return; const matches = [...($(key[0])?.querySelectorAll('button, a') || [])].filter(other => controlKey(other) === key[1]); if (matches.length === 1) matches[0].focus({preventScroll: true}); }
function render() { const focused = focusKey(); renderState(); restoreFocus(focused); }
function renderState() {
  const email = data.discovery?.identity?.email;
  if (email) { $('account-email').textContent = $('account-email').title = email; $('account-avatar').textContent = email.slice(0,1).toUpperCase(); }
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
  $('machine-grid').replaceChildren(); $('shared-grid').replaceChildren(); $('pool-grid').replaceChildren(); $('shared-view').hidden = !(data.shared || []).length; $('pool-view').hidden = !(data.pool || []).length;
  for (const item of [...data.workspaces, ...(data.shared || []), ...(data.pool || [])]) {
    const card = element('button', undefined, 'machine-card' + (item.id === selected ? ' selected' : '')); card.dataset.machine = item.id; card.setAttribute('aria-pressed', String(item.id === selected));
    const top = element('div', undefined, 'card-top'); top.append(element('span', '▣', 'machine-icon'), element('span', item.state, `state ${['running','hibernated','stopped','failed'].includes(item.state) ? item.state : 'stopped'}`));
    const meta = element('div', undefined, 'card-meta'); meta.append(element('span', `${item.vcpu_count || 1} vCPU`), element('span', mib(item.memory_mib)));
    card.append(top, element('h3', item.id), meta, element('span', diskUsage(item), 'card-lineage'), element('span', item.source ? `Forked from ${item.source}` : (imageNames[item.image || 'alpine'] || 'Linux machine'), 'card-lineage')); if (item.node) card.append(element('span', `${item.node}${item.node_online === false ? ' · offline' : ''}`, 'card-lineage'));
    if (item.owner) card.append(element('span', item.role === 'pool-admin' ? `Owned by ${item.owner}` : `Shared by ${item.owner} · ${item.role}`, 'card-lineage'));
    const [status, warning, title] = durability(item), recovery = element('span', status, 'card-lineage' + (warning ? ' recovery-warning' : ''));
    recovery.title = title;
    card.append(recovery);
    card.addEventListener('click', () => select(item.id)); $(item.role === 'pool-admin' ? 'pool-grid' : item.owner ? 'shared-grid' : 'machine-grid').append(card);
  }
  if (!data.workspaces.length) $('machine-grid').append(element('div', 'Your first machine is one click away. Choose “New machine” to get started.', 'empty'));
  const current = machine(); $('detail').hidden = !current;
  if (current) {
    $('selected-name').textContent = current.id; $('detail-actions').replaceChildren();
    const live = current.state === 'running';
    const operations=(data.nodes||[]).find(n=>n.id===current.node)?.capabilities?.operations;
    const supports=op=>!operations || operations.includes(op);
    const owned = !current.role, admin = owned || current.role === 'admin', poolOnly = current.role === 'pool-admin';
    // Pool access permits fork, transfer and AWS settings, without a guest session.
    if (poolOnly) { if (live) $('detail-actions').append(action('Fork', () => openDialog('fork', current.id), !supports('fork'))); }
    else if (live) { $('detail-actions').append(action('Open Terminal', () => openTerminal(current.id))); if (admin) $('detail-actions').append(action('Snapshot', () => openDialog('snapshot', current.id), !supports('snapshot'))); $('detail-actions').append(action('Fork', () => openDialog('fork', current.id), !supports('fork')), action('Hibernate', () => operate({op: 'hibernate', id: current.id}), !supports('hibernate'))); if (admin) $('detail-actions').append(action('Stop', () => openDialog('stop', current.id))); }
    else if (current.state !== 'missing' && !current.unmade) { $('detail-actions').append(action(current.state === 'hibernated' ? 'Resume machine' : 'Start machine', () => operate({op: 'start', id: current.id}))); }
    if (owned && current.state === 'stopped') $('detail-actions').append(action('Resize', () => openDialog('resize', current.id), !supports('resize')));
    if (owned && data.previews) $('detail-actions').append(action('Publish port', () => openDialog('publish', current.id), !supports('tunnel')));
    if (owned) $('detail-actions').append(action('Rename', () => openDialog('rename', current.id)), action('Share', () => openDialog('share', current.id)));
    if ((admin || data.pool_admin) && data.aws_identity?.enabled) $('detail-actions').append(action('AWS role', () => openDialog('aws-role', current.id)));
    if (owned || data.pool_admin) $('detail-actions').append(action('Transfer', () => openDialog('transfer', current.id)));
    if (owned && !live) $('detail-actions').append(action('Delete', () => openDialog('delete', current.id), !supports('delete')));
    $('published-ports').replaceChildren();
    for (const port of current.published_ports || []) {
      const row = element('span', `Port ${port.port}`, 'actions'), link = element('a', 'Open preview', 'button secondary'); row.dataset.key = port.port;
      link.href = port.url; link.target = '_blank'; link.rel = 'noopener noreferrer'; row.append(link);
      if (owned) row.append(action('Unpublish', () => operate({op:'unpublish',id:current.id,port:port.port})));
      $('published-ports').append(row);
    }
    $('shared-with').replaceChildren(); $('shared-with').hidden = !(current.shared_with || []).length;
    for (const grant of current.shared_with || []) { const row = element('span', `Shared with ${grant.email} · ${grant.role} `); row.dataset.key = grant.email; row.append(action('Unshare', () => operate({op: 'unshare', id: current.id, with: grant.email}))); $('shared-with').append(row); }
    // Per-box AWS access: the attached role and when its current token expires.
    const role = current.aws_role; $('aws-role').replaceChildren(); $('aws-role').hidden = !role;
    if (role) { const until = role.token_expires_at ? ` until ${new Date(role.token_expires_at * 1000).toLocaleTimeString()}` : ''; const states = {active: `token active${until}`, pending: 'token delivered when the machine runs', error: role.error || 'not delivered', inactive: 'inactive: re-attach it'}; const row = element('span', `AWS role ${role.role_arn} · ${states[role.state] || role.state} `); if (admin || data.pool_admin) row.append(action('Detach', () => operate({op: 'aws-role', id: current.id}))); $('aws-role').append(row); }
    $('run-command').disabled = !live || busy || poolOnly; $('command').disabled = !live || busy || poolOnly;
    $('console-output').textContent = histories.get(current.id) || (live ? 'Ready. Run a command to get started.\n' : 'This machine is asleep or stopped. Start it to run commands.\n');
  }
  $('snapshot-list').replaceChildren();
  for (const item of data.snapshots.slice().reverse()) {
    const row = element('article', undefined, 'snapshot-row'), info = element('div'); row.dataset.key = `${item.workspace}/${item.name}`; info.append(element('h3', item.name), snapshotStatus(item));
    const buttons = element('div', undefined, 'actions'); if (item.state === 'ready') buttons.append(action('Fork a machine', () => openDialog('fork', item.workspace, item.name)), action('Restore', () => openDialog('restore', item.workspace, item.name)));
    row.append(info, buttons); $('snapshot-list').append(row);
  }
  if (!data.snapshots.length) $('snapshot-list').append(element('div', 'No checkpoints yet. Select a running machine and capture a snapshot.', 'empty'));
  $('audit-list').replaceChildren(); $('audit-view').hidden = !(data.audit || []).length;
  for (const entry of data.audit || []) {
    const what = {transfer: `transferred ${entry.machine} from ${entry.from} to ${entry.to}${entry.keep_access ? '; the old owner kept access' : ''}`, create: `created ${entry.machine} for ${entry.to}`, fork: `forked ${entry.from}/${entry.source} into ${entry.to}/${entry.machine}`}[entry.action];
    const row = element('p', `${entry.by} ${what} · ${ago(entry.at)}`); row.title = fullTime(entry.at); $('audit-list').append(row);
  }
  $('host-node-list').replaceChildren();
  if(data.host_controls) for(const node of (data.nodes||[]).filter(n=>!n.revoked)) {
    const row=element('div'); row.dataset.key=node.id; row.append(element('span',`${node.id} · ${node.online?'online':'offline'} · ${mib(node.memory_mib)} `),action('Adjust budget',()=>hostDialog('budget',node.id))); $('host-node-list').append(row);
  }
  $('pool-aws-roles').replaceChildren();
  if (data.host_controls) for (const box of data.aws_boxes || []) {
    const row = element('div'); row.append(element('span', `${box.id} · ${box.aws_role?.role_arn || 'No AWS role'} `), action('AWS role', () => openDialog('aws-role', box.id)));
    if (box.aws_role) row.append(action('Detach', () => operate({op: 'aws-role', id: box.id})));
    $('pool-aws-roles').append(row);
  }
  $('create').disabled = busy || !loaded;
}
// An email input; optional ones may stay empty.
function emailField(name, label, required = true) { const wrapper = field(name, label, '', null), input = wrapper.querySelector('input'); input.type = 'email'; input.removeAttribute('pattern'); input.removeAttribute('title'); input.maxLength = 254; input.required = required; return wrapper; }
function field(name, label, value, choices) { const wrapper = element('label', label, 'field'); let input; if (choices) { input = element('select'); for (const [v, title, disabled] of choices) { const option = element('option', title); option.value = v; option.disabled = !!disabled; input.append(option); } } else { input = element('input'); input.pattern = '[A-Za-z0-9_\\-]{1,32}'; input.title = 'Letters, digits, - and _, up to 32 characters.'; input.maxLength = 32; input.required = true; input.autocomplete = 'off'; } input.name = name; if (value) input.value = value; wrapper.append(input); return wrapper; }
function openDialog(op, id, snapshot) {
  if (busy) return; dialogOperation = {op, id, snapshot}; $('dialog-fields').replaceChildren(); $('dialog-error').hidden = true;
  const defaults = {publish: ['Publish a port.', 'Open a guest HTTP server over tailnet HTTPS. Only you and teammates with a share can access it. The server must listen on 0.0.0.0; preview cookies are disabled.', 'Publish port'], resize: ['Grow this machine.', 'The machine must be stopped. Disk growth is permanent; its filesystem expands on the next start.', 'Resize machine'], create: ['A fresh place to build.', 'Start a persistent Linux machine. Your disk stays with it when you stop.', 'Create machine'], snapshot: ['Save this moment.', 'Capture this machine’s memory and disk together. You can return to this checkpoint later.', 'Capture snapshot'], fork: ['Try another direction.', 'Create an independent machine from this checkpoint. Changes stay separate from the parent.', 'Create fork'], restore: ['Return to this checkpoint?', 'This replaces the machine’s current memory and disk with the saved snapshot. Changes made since that snapshot will be lost.', 'Restore snapshot'], stop: ['Stop this machine?', 'Disk changes are saved. Running processes and memory state are lost. Choose Hibernate instead to keep them.', 'Stop machine'], share: ['Share this machine.', 'Your teammate gets in with their own login. A fork they make copies everything on this machine, including your credentials, and stays theirs after you unshare.', 'Share machine'], 'aws-role': ['Give this machine AWS access.', 'Programs in this machine get short-lived credentials for the role through the standard AWS SDK and CLI chain; no AWS key is stored in it. Forks start without a role. Only roles your server operator allows are listed.', 'Attach role'], transfer: ['Give this machine away.', 'The new owner gets it under the same name, with its disk, snapshots and running state, and everything on it, credentials included. Its shares end; the old owner keeps access only if you choose so.', 'Transfer machine'], rename: ['Rename this machine.', 'Only the name changes: disk, snapshots, shares, previews and running state stay. SSH config and teammates use the new name from now on.', 'Rename machine'], delete: ['Delete this machine?', 'Its disk and every snapshot taken from it are removed for good, and anyone it is shared with loses it. Forks made earlier stay.', 'Delete machine']};
  const destructive = op === 'delete' || op === 'restore'; $('dialog-eyebrow').textContent = destructive ? 'CANNOT BE UNDONE' : 'YOUR SANDBOX'; $('dialog-eyebrow').classList.toggle('warning', destructive); $('dialog-submit').className = `button ${destructive ? 'danger' : 'primary'}`;
  $('dialog-title').textContent = defaults[op][0]; $('dialog-description').textContent = defaults[op][1]; $('dialog-submit').textContent = defaults[op][2]; $('dialog-submit').disabled = false;
  const images = imageChoices();
  if (op === 'create') $('dialog-fields').append(field('id', 'Machine name', '', null), field('image', 'Operating system', images.find(choice => !choice[2])?.[0] || 'alpine', images), field('memory_mib', 'Memory', '256', [['256','256 MiB'],['512','512 MiB'],['1024','1 GiB'],['2048','2 GiB'],['4096','4 GiB'],['8192','8 GiB'],['16384','16 GiB']]), field('vcpu_count', 'vCPU', '1', Array.from({length:16},(_,i)=>[String(i+1),String(i+1)])), field('node', 'Pool node', '', [['','Automatic placement'], ...nodeChoices()]));
  if (op === 'create' || op === 'resize') {
    const size = op === 'resize' ? Math.ceil((machine()?.disk_bytes || 0) / 1073741824) : 32;
    const disk = field('disk_gib', 'Root disk (GiB)', String(size || 32));
    const input = disk.querySelector('input'); input.type = 'number'; input.removeAttribute('pattern'); input.removeAttribute('title'); input.removeAttribute('maxlength'); input.min = op === 'create' ? '1' : String(Math.max(1, size)); input.max = '16384'; input.step = '1';
    $('dialog-fields').append(disk);
  }
  if (op === 'create') {
    const form=$('operation-form'), image=form.elements.image, memory=form.elements.memory_mib, node=form.elements.node;
    const fitShape=()=>{const mac=image.value==='ubuntu-arm64'; if(mac && memory.value && Number(memory.value)<512) memory.value='512'; form.elements.disk_gib.disabled=mac; form.elements.disk_gib.parentElement.hidden=mac;};
    const vcpu=form.elements.vcpu_count, pool=data.discovery?.nodes?.length ? data.discovery : null;
    // Offer only what some eligible node can place: the chosen node (or any, when automatic) must
    // have the image ready and room for memory and vCPU together. A lone local host decides itself.
    const ready=(name,id)=>pool.images?.find(i=>i.name===name)?.nodes?.some(c=>c.node===id&&c.availability==='available');
    const room=(n,mem,cpu)=>{const f=n.available,g=n.per_guest; return (!f||(f.memory_mib>=mem&&f.vcpus>=cpu&&f.slots>0)) && (!g?.memory_mib||g.memory_mib.includes(mem)) && (g?.max_vcpus==null||g.max_vcpus>=cpu);};
    const allow=(select,ok)=>{for(const o of select.options) o.disabled=!ok(o.value); if(!select.value || select.selectedOptions[0]?.disabled) select.value=[...select.options].find(o=>!o.disabled)?.value ?? '';};
    const constrain=()=>{
      if(pool){
        const nodes=pool.nodes.filter(n=>n.online&&!n.draining&&(!node.value||n.id===node.value));
        allow(image,v=>nodes.some(n=>ready(v,n.id)));
        const here=nodes.filter(n=>ready(image.value,n.id)), fits=(mem,cpu)=>mem>0&&cpu>0&&(image.value!=='ubuntu-arm64'||mem>=512)&&here.some(n=>room(n,mem,cpu));
        // An unplaceable pair restarts from the smallest shape that fits; then each list offers what fits with the other.
        if(!fits(+memory.value,+vcpu.value)){vcpu.value='1'; memory.value=[...memory.options].find(o=>fits(+o.value,1))?.value ?? memory.value;}
        allow(memory,v=>fits(+v,+vcpu.value)); allow(vcpu,v=>fits(+memory.value,+v));
        const placeable=!!image.value&&fits(+memory.value,+vcpu.value);
        $('dialog-submit').disabled=!placeable;
        $('dialog-error').textContent=placeable ? '' : 'No operating system and machine size are available here right now. Choose another pool node or try again later.';
        $('dialog-error').hidden=placeable;
      }
      fitShape();
    };
    for(const select of [image,memory,vcpu]) select.required=true;
    for(const select of [image,node,memory,vcpu]) select.addEventListener('change',constrain);
    constrain();
  }
  if (op === 'publish') { const port = field('port', 'Guest HTTP port', '3000'), input = port.querySelector('input'); input.type = 'number'; input.removeAttribute('pattern'); input.removeAttribute('title'); input.removeAttribute('maxlength'); input.min = '1'; input.max = '65535'; input.step = '1'; $('dialog-fields').append(port); }
  if (op === 'share') $('dialog-fields').append(emailField('with', 'Teammate email'), field('role', 'Role', 'use', [['use','Use · terminal, SSH, commands, fork'],['admin','Admin · also stop, snapshot, restore, SSH keys']]));
  if (op === 'transfer') $('dialog-fields').append(emailField('to', 'New owner email'), field('keep_access', 'Old owner', 'false', [['false','Loses access'],['true','Keeps an admin share']]));
  // Pool admins may create for anyone; owners and pool admins may fork for anyone.
  if (op === 'create' && data.pool_admin || op === 'fork' && (data.pool_admin || !machine()?.role)) $('dialog-fields').append(emailField('owner', 'Owner email (empty: you)', false));
  if (op === 'aws-role') { const target = [...data.workspaces, ...(data.shared || []), ...(data.pool || []), ...(data.aws_boxes || [])].find(box => box.id === id), roles = target?.aws_roles || data.aws_identity?.roles || []; $('dialog-fields').append(roles.length ? field('role_arn', 'IAM role', target?.aws_role?.role_arn || roles[0], roles.map(r => [r, r])) : element('p', 'Your server operator allows no AWS roles for this machine yet.', 'field-error')); $('dialog-submit').disabled = !roles.length; }
  if (op === 'rename') $('dialog-fields').append(field('name', 'New machine name', id, null));
  if (op === 'delete') { const going = data.snapshots.filter(item => item.workspace === id).map(item => item.name); $('dialog-fields').append(element('p', going.length ? `Its ${going.length} snapshot${going.length === 1 ? '' : 's'} will be deleted too: ${going.join(', ')}` : 'It has no snapshots.', going.length ? 'field-error' : 'muted')); }
  if (op === 'snapshot') $('dialog-fields').append(field('name', 'Snapshot name', `snapshot-${Date.now().toString(36)}`));
  if (op === 'fork') { $('dialog-fields').append(field('child', 'New machine name', '', null)); if (!snapshot) { const choices = [['','Capture a new snapshot now'], ...data.snapshots.filter(item => item.workspace === id).map(item => [item.name,item.name])]; $('dialog-fields').append(field('snapshot', 'Start from', '', choices)); } }
  if (id) $('dialog-fields').append(element('p', `Machine: ${id}${snapshot ? ` · Snapshot: ${snapshot}` : ''}`, 'muted'));
  $('operation-dialog').showModal();
  // Start on the first field (the machine name when creating), not the close button.
  $('dialog-fields').querySelector('input, select')?.focus();
}
async function operate(operation) {
  const {operation_key: suppliedKey, ...parameters} = operation; const intent = JSON.stringify(parameters); const storageKey = 'ow-operation:' + intent; operation.operation_key ||= sessionStorage.getItem(storageKey) || crypto.randomUUID().replaceAll('-',''); sessionStorage.setItem(storageKey, operation.operation_key);
  busy = true; render(); notice(`${operation.op === 'exec' ? 'Running command' : 'Working on your machine'}…`);
  // A result is shown only where it was started; if the user moved on, the notice would mislabel another view or machine.
  const origin = `${view}/${selected}`, here = () => `${view}/${selected}` === origin;
  try { const result = await api('/api/operation', operation); const stay = here(); if ((operation.op === 'fork' || operation.op === 'create') && !result.owner) selected = result.id; if (operation.op === 'rename' && result.id !== operation.id) { if (selected === operation.id) selected = result.id; if (histories.has(operation.id)) { histories.set(result.id, histories.get(operation.id)); histories.delete(operation.id); } } sessionStorage.removeItem(storageKey); const messages = {publish:'Port published. Open the preview below.', unpublish:'Port unpublished; preview connections closed.', resize:'Machine resized. Start it to grow the filesystem.', create:'Machine created.', start:'Machine started.', hibernate:'Machine hibernated. Memory and disk state saved.', stop:'Machine stopped. Disk changes retained.', snapshot:'Snapshot saved.', fork:'Independent fork created.', restore:'Snapshot restored.', share:'Machine shared.', transfer:`Machine transferred to ${result.owner}.`, 'aws-role': operation.role_arn ? 'AWS role attached.' : 'AWS role detached; its token was removed.', rename:'Machine renamed.', delete:'Machine deleted.', unshare:'Access removed; their open sessions were closed.'}; if (result.owner && operation.op !== 'transfer') messages[operation.op] = `${operation.op === 'fork' ? 'Fork' : 'Machine'} created for ${result.owner}.`; if (stay) notice(operation.op === 'exec' ? `Command finished · exit ${result.exit_code}` : messages[operation.op], operation.op === 'exec' && result.exit_code !== 0); return result; }
  catch (error) { if (error.operation?.state === 'failed') sessionStorage.removeItem(storageKey); if (here()) notice(error.message, true); throw error; }
  // Do not hold the caller (an open dialog) until the state reload answers; it can take a minute.
  finally { busy = false; if (machine()) render(); refresh(); }
}
const busyLabels = {create: 'Creating…', snapshot: 'Capturing…', fork: 'Forking…', restore: 'Restoring…', resize: 'Resizing…', delete: 'Deleting…'};
$('operation-form').addEventListener('submit', async event => { event.preventDefault(); if (!event.currentTarget.reportValidity()) return; const fields = Object.fromEntries(new FormData(event.currentTarget)); const op = {...dialogOperation, ...fields}; if (op.port) op.port = Number(op.port); if (op.disk_gib) op.disk_gib = Number(op.disk_gib); if (op.memory_mib) op.memory_mib = Number(op.memory_mib); if (op.vcpu_count) op.vcpu_count = Number(op.vcpu_count); if (op.op === 'resize' && op.disk_gib === Math.ceil((machine()?.disk_bytes || 0) / 1073741824)) { $('dialog-error').textContent = `Already ${op.disk_gib} GiB. Choose a larger size to grow it.`; $('dialog-error').hidden = false; return; } if (op.op === 'restore') op.name = op.snapshot; if (!op.node) delete op.node; if (!op.owner) delete op.owner; if (op.keep_access) op.keep_access = op.keep_access === 'true'; if (!op.snapshot) delete op.snapshot; if (!op.id) delete op.id; if (op.op === 'restore') delete op.snapshot; const label = $('dialog-submit').textContent; $('dialog-error').hidden = true; $('dialog-submit').textContent = busyLabels[op.op] || 'Working…'; $('dialog-submit').disabled = true; $('cancel-dialog').disabled = true; $('close-dialog').disabled = true; try { await operate(op); $('operation-dialog').close(); } catch (error) { $('dialog-error').textContent = error.message; $('dialog-error').hidden = false; } finally { $('dialog-submit').textContent = label; $('dialog-submit').disabled = false; $('cancel-dialog').disabled = false; $('close-dialog').disabled = false; } });
$('console-form').addEventListener('submit', async event => { event.preventDefault(); const current = machine(); if (!current || busy) return; const command = $('command').value.trim(); if (!command) return; let history = histories.get(current.id) || ''; history += `$ ${command}\n`; histories.set(current.id, history); try { const result = await operate({op:'exec',id:current.id,command}); history += result.output + (result.output && !result.output.endsWith('\n') ? '\n' : '') + `[exit ${result.exit_code}]\n`; histories.set(current.id, history.slice(-200000)); $('command').value = ''; render(); $('console-output').scrollTop = $('console-output').scrollHeight; } catch (error) { histories.set(current.id, history + error.message + '\n'); render(); } });
$('command').addEventListener('keydown', event => { if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); $('console-form').requestSubmit(); } });
$('clear-console').addEventListener('click', () => { if (selected) histories.delete(selected); render(); });
$('create').addEventListener('click', () => openDialog('create'));
$('refresh').addEventListener('click', refresh);
// A refused operation's error is the dialog's; it goes with the dialog.
for (const id of ['close-dialog','cancel-dialog']) $(id).addEventListener('click', () => { if (!busy) { $('operation-dialog').close(); notice(''); } });
$('operation-dialog').addEventListener('cancel', event => { if (busy) event.preventDefault(); else notice(''); });
document.querySelectorAll('.nav').forEach(button => button.addEventListener('click', () => setView(button.dataset.view)));
refresh(); setInterval(() => { if (!busy && !document.hidden && !$('operation-dialog').open) refresh(); }, 5000);

let terminalSocket = null, terminalView = null, terminalFit = null, terminalObserver = null, terminalId = null;
// Ends the session but keeps the dialog: input stops, and the user can close or start a new shell.
function endTerminal(message) {
  $('terminal-status').textContent = message;
  if (terminalSocket) { const socket = terminalSocket; terminalSocket = null; socket.close(); }
  if (terminalView) { terminalView.options.cursorBlink = false; terminalView.options.disableStdin = true; }
  $('terminal-close').textContent = 'Close'; $('terminal-new').hidden = false;
}
// A machine hibernated or stopped elsewhere may leave the socket open with nobody reading it,
// or close it first; either way the open dialog says why once the poll sees the new state.
function checkTerminal() {
  if (!$('terminal-dialog').open) return;
  const item = [...data.workspaces, ...(data.shared || [])].find(entry => entry.id === terminalId);
  const message = `Machine ${item ? item.state : 'removed'}. Start it, then choose New session.`;
  if ((!item || item.state !== 'running') && $('terminal-status').textContent !== message) endTerminal(message);
}
function closeTerminal() {
  if (terminalSocket) { terminalSocket.close(); terminalSocket = null; }
  if (terminalObserver) { terminalObserver.disconnect(); terminalObserver = null; }
  if (terminalView) { terminalView.dispose(); terminalView = null; }
  terminalFit = null; $('terminal-screen').replaceChildren();
  if ($('terminal-dialog').open) $('terminal-dialog').close();
}
function openTerminal(id) {
  closeTerminal(); terminalId = id; $('terminal-title').textContent = `${id} / Terminal`; $('terminal-status').textContent = 'Connecting…';
  $('terminal-close').textContent = 'Disconnect'; $('terminal-new').hidden = true;
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
      const control = JSON.parse(event.data); if (control.type === 'exit') endTerminal(`Shell exited · ${control.code}`);
    } else {
      const bytes = new Uint8Array(event.data); terminal.write(bytes, () => { if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify({type:'ack',bytes:bytes.length})); });
    }
  };
  socket.onerror = () => { if (terminalSocket === socket) endTerminal('Connection failed. Check the machine and reload to sign in.'); };
  socket.onclose = () => { if (terminalSocket === socket) endTerminal('Disconnected.'); };
  terminal.onData(text => {
    if (socket.readyState !== WebSocket.OPEN) return;
    const bytes = new TextEncoder().encode(text);
    if (socket.bufferedAmount + bytes.length > 65536) { endTerminal('Input exceeded connection capacity. Start a new session to continue.'); return; }
    for (let offset=0; offset<bytes.length; offset+=4096) socket.send(bytes.slice(offset,offset+4096));
  });
}
$('terminal-close').addEventListener('click', closeTerminal);
$('terminal-new').addEventListener('click', () => openTerminal(terminalId));
$('terminal-dialog').addEventListener('cancel', event => { event.preventDefault(); closeTerminal(); });
window.addEventListener('pagehide', closeTerminal);

const installCommand = `curl -fsSL ${location.origin}/cli/install.sh | sh`;
$('cli-command').textContent = installCommand;
$('cli-login').textContent = `ow login ${location.origin}`;
// Tailnet sign-in serves no CLI installer: the CLI signs in through Access only.
fetch('/cli/install.sh', {cache: 'no-store'}).then(r => { if (!r.ok) $('cli-install').hidden = true; }).catch(() => { $('cli-install').hidden = true; });
// Hidden until the server confirms it serves the binary, so the link never downloads a 404 page.
fetch('/cli/ow-linux-amd64', {method: 'HEAD', cache: 'no-store'}).then(r => { $('cli-download').hidden = !r.ok; }).catch(() => {});
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

import {test, expect, api, machines, card, detailAction, dialog, notice} from './fixtures.mjs';

test('machine list shows cards, counts and the selected machine', async ({dashboard: page}) => {
  await machines(page, 'alpha', 'beta');
  await expect(page.locator('.machine-card')).toHaveCount(2);
  await expect(page.locator('#nav-count')).toHaveText('2');
  await expect(page.locator('#metric-running')).toHaveText('2');
  await expect(card(page, 'alpha')).toContainText('running');
  await expect(card(page, 'alpha')).toContainText('256 MiB');
  await expect(card(page, 'alpha')).toContainText('32.0 GiB disk');
  // The first machine is selected; clicking another selects it instead.
  await expect(card(page, 'alpha')).toHaveAttribute('aria-pressed', 'true');
  await card(page, 'beta').click();
  await expect(card(page, 'beta')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('#selected-name')).toHaveText('beta');
});

test('empty sandbox invites a first machine', async ({dashboard: page}) => {
  await expect(page.locator('#machine-grid .empty')).toHaveText(/Your first machine is one click away/);
  await expect(page.locator('#nav-count')).toHaveText('0');
  await expect(page.locator('#detail')).toBeHidden();
  await page.getByRole('button', {name: /Snapshots/}).click();
  await expect(page.locator('#snapshot-list .empty')).toHaveText(/No checkpoints yet/);
});

test('memory counter shows your usage against your owner limits', async ({dashboard: page}) => {
  await expect(page.locator('#metric-reserved')).toHaveText('0 MiB of 32.0 GiB');
  await machines(page, 'counted');
  // The harness uses the default owner caps: 8 machines, 16 vCPU, 32 GiB memory, 256 GiB disk.
  await expect(page.locator('#metric-reserved')).toHaveText('256 MiB of 32.0 GiB');
  await expect(page.locator('#metric-limit')).toHaveText('1 of 8 machines · 1 of 16 vCPU · 32 of 256 GiB disk');
});

test('create dialog creates a machine with the chosen shape', async ({dashboard: page}) => {
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(dialog(page)).toBeVisible();
  await expect(page.locator('#dialog-title')).toHaveText('A fresh place to build.');
  await expect(page.locator('#dialog-eyebrow')).toHaveText('YOUR SANDBOX');
  await expect(page.locator('#dialog-submit')).toHaveClass('button primary');
  await page.getByLabel('Machine name').fill('made-in-ui');
  await page.getByLabel('Operating system').selectOption('ubuntu');
  await page.getByLabel('Memory').selectOption('512');
  await page.locator('select[name=vcpu_count]').selectOption('2');
  await page.getByLabel('Root disk (GiB)').fill('40');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(dialog(page)).toBeHidden();
  await expect(notice(page)).toHaveText('Machine created.');
  await expect(card(page, 'made-in-ui')).toContainText('2 vCPU');
  await expect(card(page, 'made-in-ui')).toContainText('512 MiB');
  await expect(card(page, 'made-in-ui')).toContainText('40.0 GiB disk');
  await expect(card(page, 'made-in-ui')).toContainText('Ubuntu 24.04');
  await expect(page.locator('#selected-name')).toHaveText('made-in-ui');
});

test('create dialog shows a refused create and stays open', async ({dashboard: page}) => {
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await page.getByLabel('Machine name').fill('too-big');
  // 16 GiB is more than this host's 4 GiB of capacity.
  await page.getByLabel('Memory').selectOption('16384');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toBeVisible();
  await expect(page.locator('#dialog-error')).not.toBeEmpty();
  await expect(dialog(page)).toBeVisible();
  await expect(notice(page)).toHaveClass(/error/);
  await page.getByRole('button', {name: 'Cancel'}).click();
  await expect(dialog(page)).toBeHidden();
  await expect(card(page, 'too-big')).toBeVisible();
});

// A pool as /api/state discovery reports it: `ready` names the nodes each image is
// prepared on (other images and nodes are missing_image). The e2e host itself is a lone
// local worker with no pool nodes, so these tests answer discovery from here.
const free = (id, memory_mib, vcpus, slots = 2) => ({id, online: true, draining: false, available: {memory_mib, vcpus, slots}, workspaces: {available: 10}});
// The task's two-node x86-64 pool: node-b has no free vCPU or slot; arch and ubuntu-arm64 are prepared nowhere.
const twoNodes = {nodes: [free('node-a', 3072, 3), free('node-b', 1024, 0, 0)], ready: {alpine: ['node-a', 'node-b'], ubuntu: ['node-a', 'node-b']}};
async function twoNodePool(page, {nodes, ready} = twoNodes) {
  await page.route('**/api/state', async route => {
    const response = await route.fetch({url: `https://127.0.0.1:${process.env.OW_E2E_PORT || 18443}/api/state`, headers: {...await route.request().allHeaders(), host: 'dashboard.e2e.ts.net'}});
    const body = await response.json();
    body.result.discovery.nodes = nodes;
    body.result.discovery.images = ['alpine', 'arch', 'ubuntu', 'ubuntu-arm64'].map(name => {
      const choices = nodes.map(n => ({node: n.id, availability: !(ready[name] || []).includes(n.id) ? 'missing_image' : n.available.slots ? 'available' : 'insufficient_capacity'}));
      return {name, available: choices.some(c => c.availability === 'available'), nodes: choices};
    });
    await route.fulfill({response, json: body});
  });
  await page.reload();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
}
// Answers creates without placing them and returns what the dialog sent.
function createsSent(page) {
  const sent = [];
  page.route('**/api/operation', route => { sent.push(route.request().postDataJSON()); return route.fulfill({status: 409, json: {ok: false, error_code: 'insufficient_capacity', error: 'refused in test'}}); });
  return sent;
}
const option = (page, field, value) => page.locator(`select[name=${field}] option[value="${value}"]`);

test('create dialog marks images the pool cannot run', async ({dashboard: page}) => {
  await twoNodePool(page);
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(option(page, 'image', 'arch')).toHaveText('Arch Linux · not available on this pool');
  await expect(option(page, 'image', 'arch')).toHaveJSProperty('disabled', true);
  await expect(option(page, 'image', 'ubuntu-arm64')).toHaveText('Ubuntu 24.04 ARM64 · not available on this pool');
  await expect(option(page, 'image', 'ubuntu-arm64')).toHaveJSProperty('disabled', true);
  await expect(option(page, 'image', 'ubuntu')).toHaveJSProperty('disabled', false);
  await expect(page.getByLabel('Operating system')).toHaveValue('alpine');
});

test('create dialog shows free room per node, disables a full one and explains a refusal plainly', async ({dashboard: page}) => {
  await twoNodePool(page);
  // The runner answers a create that no longer fits with this code and a CLI-oriented text.
  await page.route('**/api/operation', route => route.fulfill({status: 409, json: {ok: false, error_code: 'insufficient_capacity', error: 'Placement or disk capacity is exhausted. Check ow nodes and image list; wait or choose fitting resources. Inspect the original operation before retrying; reuse its key after an uncertain response.'}}));
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(option(page, 'node', 'node-a')).toHaveText('node-a · 3 vCPU · 3.0 GiB free');
  await expect(option(page, 'node', 'node-a')).toHaveJSProperty('disabled', false);
  await expect(option(page, 'node', 'node-b')).toHaveText('node-b · full');
  await expect(option(page, 'node', 'node-b')).toHaveJSProperty('disabled', true);
  await page.getByLabel('Machine name').fill('no-room');
  await page.getByLabel('Pool node').selectOption('node-a');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toHaveText('The pool has no room for a machine this size right now. Choose less memory or fewer vCPUs, another node, or try again later.');
  await expect(dialog(page)).toBeVisible();
});

test('create dialog offers only sizes the chosen node can place', async ({dashboard: page}) => {
  await twoNodePool(page);
  const sent = createsSent(page);
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await page.getByLabel('Pool node').selectOption('node-a');
  // node-a has 3 vCPU and 3 GiB free.
  await expect(option(page, 'vcpu_count', '3')).toHaveJSProperty('disabled', false);
  await expect(option(page, 'vcpu_count', '4')).toHaveJSProperty('disabled', true);
  await expect(option(page, 'vcpu_count', '16')).toHaveJSProperty('disabled', true);
  await expect(option(page, 'memory_mib', '2048')).toHaveJSProperty('disabled', false);
  await expect(option(page, 'memory_mib', '4096')).toHaveJSProperty('disabled', true);
  await expect(option(page, 'memory_mib', '16384')).toHaveJSProperty('disabled', true);
  await page.locator('select[name=vcpu_count]').selectOption('3');
  await page.getByLabel('Memory').selectOption('2048');
  await page.getByLabel('Machine name').fill('fits');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toBeVisible();
  expect(sent).toMatchObject([{op: 'create', node: 'node-a', memory_mib: 2048, vcpu_count: 3}]);
});

test('create dialog offers only images the chosen node has ready', async ({dashboard: page}) => {
  await twoNodePool(page, {nodes: [free('node-a', 4096, 4), free('node-c', 4096, 4)], ready: {alpine: ['node-a', 'node-c'], ubuntu: ['node-a']}});
  const sent = createsSent(page);
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await page.getByLabel('Operating system').selectOption('ubuntu');
  await page.getByLabel('Pool node').selectOption('node-c');
  // Ubuntu is not prepared on node-c, so the dialog falls back to an image it has.
  await expect(option(page, 'image', 'ubuntu')).toHaveJSProperty('disabled', true);
  await expect(page.getByLabel('Operating system')).toHaveValue('alpine');
  await page.getByLabel('Machine name').fill('on-c');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toBeVisible();
  expect(sent).toMatchObject([{node: 'node-c', image: 'alpine'}]);
  await page.getByLabel('Pool node').selectOption('node-a');
  await expect(option(page, 'image', 'ubuntu')).toHaveJSProperty('disabled', false);
});

test('create dialog blocks a full pool without submitting a shape', async ({dashboard: page}) => {
  await twoNodePool(page, {nodes: [free('node-a', 0, 0, 0), free('node-b', 0, 0, 0)], ready: twoNodes.ready});
  const sent = createsSent(page);
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await page.getByLabel('Machine name').fill('no-room');
  for (const field of ['image', 'memory_mib', 'vcpu_count']) {
    await expect(page.locator(`select[name=${field}] option:enabled`)).toHaveCount(0);
    await expect(page.locator(`select[name=${field}]`)).toHaveValue('');
  }
  await expect(page.getByRole('button', {name: 'Create machine'})).toBeDisabled();
  await expect(page.locator('#dialog-error')).toContainText('No operating system and machine size are available');
  // Enter and a direct submit event must not send defaults for the empty selects.
  await page.getByLabel('Machine name').press('Enter');
  await page.locator('#operation-form').evaluate(form => form.dispatchEvent(new Event('submit', {bubbles: true, cancelable: true})));
  expect(sent).toEqual([]);
  await expect(dialog(page)).toBeVisible();
});

test('create dialog blocks a node without images and recovers on a ready node', async ({dashboard: page}) => {
  await twoNodePool(page, {nodes: [free('node-a', 4096, 4), free('node-c', 4096, 4)], ready: {ubuntu: ['node-a']}});
  const sent = createsSent(page);
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(page.getByLabel('Operating system')).toHaveValue('ubuntu');
  await page.getByLabel('Machine name').fill('no-image');
  await page.getByLabel('Pool node').selectOption('node-c');
  for (const field of ['image', 'memory_mib', 'vcpu_count']) {
    await expect(page.locator(`select[name=${field}] option:enabled`)).toHaveCount(0);
    await expect(page.locator(`select[name=${field}]`)).toHaveValue('');
  }
  await expect(page.getByRole('button', {name: 'Create machine'})).toBeDisabled();
  await expect(page.locator('#dialog-error')).toBeVisible();
  await page.getByLabel('Machine name').press('Enter');
  await page.locator('#operation-form').evaluate(form => form.dispatchEvent(new Event('submit', {bubbles: true, cancelable: true})));
  expect(sent).toEqual([]);
  await page.getByLabel('Pool node').selectOption('node-a');
  await expect(page.getByRole('button', {name: 'Create machine'})).toBeEnabled();
  await expect(page.locator('#dialog-error')).toBeHidden();
  await expect(page.getByLabel('Operating system')).toHaveValue('ubuntu');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toBeVisible();
  expect(sent).toMatchObject([{node: 'node-a', image: 'ubuntu', memory_mib: 256, vcpu_count: 1}]);
});

test('create dialog starts an ARM-only pool on a shape its Mac runner accepts', async ({dashboard: page}) => {
  await twoNodePool(page, {nodes: [{...free('mac-a', 4096, 4), per_guest: {memory_mib: [512, 1024, 2048], max_vcpus: 4}}], ready: {'ubuntu-arm64': ['mac-a']}});
  const sent = createsSent(page);
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(page.getByLabel('Operating system')).toHaveValue('ubuntu-arm64');
  await expect(page.getByLabel('Memory')).toHaveValue('512');
  await expect(option(page, 'memory_mib', '256')).toHaveJSProperty('disabled', true);
  await expect(page.getByLabel('Root disk (GiB)')).toBeHidden();
  await page.getByLabel('Machine name').fill('arm');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toBeVisible();
  expect(sent).toHaveLength(1);
  expect(sent[0]).toMatchObject({image: 'ubuntu-arm64', memory_mib: 512});
  expect(sent[0]).not.toHaveProperty('disk_gib');
});

test('create dialog says a name is taken and stays open', async ({dashboard: page}) => {
  await machines(page, 'taken');
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await page.getByLabel('Machine name').fill('taken');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toHaveText('You already have a machine with that name. Choose another name.');
  await expect(dialog(page)).toBeVisible();
  await expect(page.locator('.machine-card')).toHaveCount(1);
});

test('create dialog starts on the machine name', async ({dashboard: page}) => {
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(page.getByLabel('Machine name')).toBeFocused();
});

test('rename keeps the machine selected under its new name', async ({dashboard: page}) => {
  await machines(page, 'old-name');
  await page.locator('#command').fill('echo before-rename');
  await page.getByRole('button', {name: 'Run ↵'}).click();
  await expect(page.locator('#console-output')).toContainText('before-rename');
  await detailAction(page, 'Rename').click();
  await expect(page.locator('#dialog-description')).toHaveText(/Only the name changes/);
  await page.getByLabel('New machine name').fill('new-name');
  await page.getByRole('button', {name: 'Rename machine'}).click();
  await expect(notice(page)).toHaveText('Machine renamed.');
  await expect(card(page, 'new-name')).toBeVisible();
  await expect(card(page, 'old-name')).toHaveCount(0);
  await expect(page.locator('#selected-name')).toHaveText('new-name');
  // The console history follows the machine.
  await expect(page.locator('#console-output')).toContainText('before-rename');
  expect((await api(page)).workspaces.map(m => m.id)).toEqual(['new-name']);
});

test('navigation switches between machines, snapshots and usage', async ({dashboard: page}) => {
  await machines(page, 'metered');
  await page.getByRole('button', {name: /Resource usage/}).click();
  await expect(page.locator('#page-title')).toHaveText('Room to grow.');
  await expect(page.locator('#breadcrumb')).toHaveText('Your sandbox / Resource usage');
  await expect(page.locator('#usage-view')).toBeVisible();
  await expect(page.locator('#machines-view')).toBeHidden();
  await expect(page.locator('#capacity-text')).toHaveText(/\/ 4\.0 GiB$/);
  await page.getByRole('button', {name: /Snapshots/}).click();
  await expect(page.locator('#page-title')).toHaveText('Pick up from here.');
  await page.getByRole('button', {name: /Machines/}).click();
  await expect(page.locator('#machines-view')).toBeVisible();
});

test('cards keep their content at the top, whatever their height', async ({dashboard: page}) => {
  await machines(page, 'short', 'tall');
  // A taller neighbour in the same row stretches every card; content must not drift down.
  await card(page, 'tall').evaluate(node => node.append(Object.assign(document.createElement('span'), {className: 'card-lineage', textContent: 'extra line'})));
  const offsets = await page.locator('.machine-card').evaluateAll(cards => cards.map(c => c.querySelector('.machine-icon').getBoundingClientRect().top - c.getBoundingClientRect().top));
  expect(offsets).toHaveLength(2);
  // 1 px border + 21 px padding: the icon sits at the top, not centred.
  for (const offset of offsets) expect(offset).toBeCloseTo(22, 0);
  // The notice keeps a gap from the card below it.
  await card(page, 'short').click();
  await detailAction(page, 'Hibernate').click();
  await expect(notice(page)).toBeVisible();
  const gap = await notice(page).evaluate(node => {
    let next = node.nextElementSibling; while (next.hidden) next = next.nextElementSibling;
    return next.getBoundingClientRect().top - node.getBoundingClientRect().bottom;
  });
  expect(gap).toBeGreaterThanOrEqual(16);
});

test('on a phone, tapping a card brings its actions into view', async ({dashboard: page}) => {
  await page.setViewportSize({width: 390, height: 844});
  await machines(page, 'm1', 'm2', 'm3', 'm4', 'm5');
  await card(page, 'm1').evaluate(node => node.scrollIntoView({block: 'start'}));
  await expect(page.locator('.detail-heading')).not.toBeInViewport();
  await card(page, 'm1').click();
  await expect(page.locator('#selected-name')).toHaveText('m1');
  await expect(page.locator('.detail-heading')).toBeInViewport();
  await expect(detailAction(page, 'Hibernate')).toBeInViewport();
});

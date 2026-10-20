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

test('create dialog creates a machine with the chosen shape', async ({dashboard: page}) => {
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(dialog(page)).toBeVisible();
  await expect(page.locator('#dialog-title')).toHaveText('A fresh place to build.');
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

// A two-node x86-64 pool as /api/state discovery reports it: arch and ubuntu-arm64 are
// prepared on no node, and node-b has no free vCPU or slot. The e2e host itself is a
// lone local worker with no pool nodes, so this test answers discovery from here.
async function twoNodePool(page) {
  await page.route('**/api/state', async route => {
    const response = await route.fetch({url: `https://127.0.0.1:${process.env.OW_E2E_PORT || 18443}/api/state`, headers: {...await route.request().allHeaders(), host: 'dashboard.e2e.ts.net'}});
    const body = await response.json();
    body.result.discovery.nodes = [
      {id: 'node-a', online: true, draining: false, available: {memory_mib: 3072, vcpus: 3, slots: 2}, workspaces: {available: 10}},
      {id: 'node-b', online: true, draining: false, available: {memory_mib: 1024, vcpus: 0, slots: 0}, workspaces: {available: 10}},
    ];
    body.result.discovery.images = [['alpine', true], ['arch', false], ['ubuntu', true], ['ubuntu-arm64', false]].map(([name, available]) => ({name, available, nodes: []}));
    await route.fulfill({response, json: body});
  });
  await page.reload();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
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

import {test, expect, signIn, notice, api} from './fixtures.mjs';

test('host pool owner invites, budgets and revokes a Linux host', async ({context, page}) => {
  // Only the configured pool owner gets host controls.
  await signIn(context, 'owner');
  await page.goto('/');
  await expect(page.locator('#host-controls')).toBeVisible();
  // Machine users get pool copy; the invitation policy stays in the invite dialog.
  await expect(page.locator('#pool-policy')).toHaveText(/can inspect its data\.$/);
  await expect(page.locator('#pool-policy')).not.toContainText('invitation');

  await page.getByRole('button', {name: 'Invite Linux host'}).click();
  const form = page.locator('#host-dialog');
  await expect(form.locator('#host-title')).toHaveText('Invite a trusted Linux host');
  await expect(form.locator('#host-description')).toContainText('This invitation grants node participation only');
  const node = form.getByLabel('Node label');
  const sent = [];
  page.on('request', request => { if (request.url().endsWith('/api/hosts/invite')) sent.push(request); });
  await node.fill('bad name!');
  await form.getByRole('button', {name: 'Create invitation'}).click();
  expect(await node.evaluate(input => input.validity.patternMismatch)).toBe(true);
  await expect(node).toHaveAttribute('title', 'Letters, digits, - and _, up to 64 characters.');
  expect(sent).toEqual([]);
  await node.fill('a'.repeat(64));
  expect(await node.evaluate(input => input.validity.valid)).toBe(true);
  await node.fill('a'.repeat(65));
  expect(await node.evaluate(input => input.validity.patternMismatch)).toBe(true);
  await node.fill('e2e-node');
  await form.getByRole('button', {name: 'Create invitation'}).click();
  await expect(form.locator('#host-envelope')).toBeVisible();
  expect(JSON.parse(await form.locator('#host-envelope').inputValue())).toBeTruthy();
  await expect(form.locator('#host-instructions')).toBeVisible();
  // Closing the dialog clears the private invitation.
  await form.getByRole('button', {name: 'Close'}).click();
  await expect(form.locator('#host-envelope')).toHaveValue('');

  await page.getByRole('button', {name: 'Adjust host budget'}).click();
  await expect(form.locator('#host-title')).toHaveText('Adjust enrolled host budget');
  await form.getByLabel('Node label').fill('e2e-node');
  await form.getByRole('button', {name: 'Save owner limits'}).click();
  // An invited host that never enrolled has no inventory, so its budget is refused.
  await expect(form.locator('#host-error')).toHaveText(/Inspect pool state before retrying\.$/);
  await form.getByRole('button', {name: 'Close'}).click();

  page.once('dialog', prompt => prompt.accept('e2e-node'));
  await page.getByRole('button', {name: 'Revoke host'}).click();
  await expect(notice(page)).toHaveText('Node revoked. Ask its operator to stop participation.');
});

test('host controls are hidden from other users', async ({dashboard: page}) => {
  await expect(page.locator('#host-controls')).toBeHidden();
});

test('fleet health shows node metrics and alerts', async ({context, page}) => {
  await signIn(context, 'owner');
  await page.goto('/');
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
  // The real controller state carries fleet health for the pool owner (no runner enrolled here).
  const state = await api(page);
  expect(state.fleet.alerts).toEqual([]);
  expect(state.fleet.settings['telemetry.enabled']).toBe(true);
  // One runner reporting telemetry and one that went offline, as the controller reports them.
  const now = Math.floor(Date.now() / 1000), gib = 2 ** 30;
  const nodes = [
    {id: 'runner-a', online: true, revoked: false, memory_mib: 4096},
    {id: 'runner-b', online: false, revoked: false, memory_mib: 4096},
  ];
  const fleet = {
    alerts: [{kind: 'node_offline', node: 'runner-b', severity: 'critical', message: 'runner runner-b has not reached the controller since 600 s ago'}],
    nodes: [
      {id: 'runner-a', online: true, telemetry_v1: true, telemetry: {unix_s: now, metrics: {'system.cpu.utilization': 0.25, 'system.memory.limit': 8 * gib, 'system.memory.usage{system.memory.state=used}': 2 * gib, 'system.cpu.load_average.1m': 1.5, 'system.filesystem.usage{system.filesystem.state=free}': 100 * gib, 'process.memory.usage{ow.process=agent}': 24 * 2 ** 20, 'ow.node.channel.connects': 2}},
        events: [{unix_s: now, severity: 'INFO', name: 'ow.node.channel.reconnected', attributes: {reason: 'connect'}}]},
      {id: 'runner-b', online: false, telemetry_v1: false, telemetry: null, events: []},
    ],
  };
  await page.route('**/api/state', async route => {
    const response = await route.fetch({url: `https://127.0.0.1:${process.env.OW_E2E_PORT || 18443}/api/state`, headers: {...await route.request().allHeaders(), host: 'dashboard.e2e.ts.net'}});
    const body = await response.json();
    Object.assign(body.result, {nodes, fleet});
    await route.fulfill({response, json: body});
  });
  await page.reload();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
  await expect(page.locator('#fleet-alerts')).toBeVisible();
  await expect(page.locator('#fleet-alerts')).toHaveText('runner runner-b has not reached the controller since 600 s ago');
  const health = page.locator('#host-node-list [data-key="runner-a"] .node-health');
  await expect(health).toHaveText('CPU 25% · memory 2.0 GiB of 8.0 GiB · load 1.50 · disk 100.0 GiB free · agent 24 MiB · 2 channel connects · updated just now · last event: node.channel.reconnected (connect) just now');
  await expect(page.locator('#host-node-list [data-key="runner-b"] .node-health')).toHaveText('no telemetry from this runner version');
  // Without alerts the banner goes away.
  fleet.alerts = [];
  await page.reload();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
  await expect(page.locator('#fleet-alerts')).toBeHidden();
});

test('fleet health is hidden from other users', async ({dashboard: page}) => {
  expect((await api(page)).fleet).toBeUndefined();
  await expect(page.locator('#fleet-alerts')).toBeHidden();
});

import {test, expect, machines, detailAction, dialog, notice} from './fixtures.mjs';

test('publish port is hidden when previews are not configured', async ({dashboard: page}) => {
  await machines(page, 'no-previews');
  await expect(detailAction(page, 'Open Terminal')).toBeVisible();
  await expect(detailAction(page, 'Publish port')).toHaveCount(0);
});

// Previews need `tailscale serve` and a root-owned helper on the host, so this test
// keeps the real machine list and answers only the preview parts of the API.
test('publish port dialog publishes, links and unpublishes a preview', async ({dashboard: page}) => {
  await machines(page, 'web');
  let published = [], sent = [];
  await page.route('**/api/state', async route => {
    // The test runner cannot resolve the dashboard name, so fetch through its listener.
    const response = await route.fetch({url: `https://127.0.0.1:${process.env.OW_E2E_PORT || 18443}/api/state`, headers: {...await route.request().allHeaders(), host: 'dashboard.e2e.ts.net'}});
    const body = await response.json();
    body.result.previews = true;
    for (const machine of body.result.workspaces) machine.published_ports = machine.id === 'web' ? published : [];
    await route.fulfill({response, json: body});
  });
  await page.route('**/api/operation', async route => {
    const operation = route.request().postDataJSON();
    if (!['publish', 'unpublish'].includes(operation.op)) return route.continue();
    sent.push(operation);
    published = operation.op === 'publish' ? [{port: operation.port, https_port: 11000, url: 'https://dashboard.e2e.ts.net:11000/'}] : [];
    await route.fulfill({json: {ok: true, result: {id: operation.id, published_ports: published}}});
  });
  await page.reload();
  await detailAction(page, 'Publish port').click();
  await expect(page.locator('#dialog-title')).toHaveText('Publish a port.');
  await expect(page.locator('#dialog-description')).toHaveText(/must listen on 0\.0\.0\.0/);
  await expect(page.getByLabel('Guest HTTP port')).toHaveValue('3000');
  await page.getByLabel('Guest HTTP port').fill('8080');
  await dialog(page).getByRole('button', {name: 'Publish port'}).click();
  await expect(notice(page)).toHaveText('Port published. Open the preview below.');
  expect(sent[0]).toMatchObject({op: 'publish', id: 'web', port: 8080});
  const link = page.locator('#published-ports').getByRole('link', {name: 'Open preview'});
  await expect(link).toHaveAttribute('href', 'https://dashboard.e2e.ts.net:11000/');
  await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
  await expect(page.locator('#published-ports')).toContainText('Port 8080');
  await page.locator('#published-ports').getByRole('button', {name: 'Unpublish'}).click();
  await expect(notice(page)).toHaveText('Port unpublished; preview connections closed.');
  expect(sent[1]).toMatchObject({op: 'unpublish', id: 'web', port: 8080});
  await expect(page.locator('#published-ports')).toBeEmpty();
});

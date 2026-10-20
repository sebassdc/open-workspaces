import {test, expect, machines, detailAction, dialog, notice} from './fixtures.mjs';

test('no console errors on page load and dialog open', async ({page}) => {
  const errors = [];
  // Network failures are logged too; tailnet sign-in answers the CLI installer and binary probes with 404 on purpose.
  page.on('console', message => { if (['error', 'warning'].includes(message.type()) && !(message.text().includes('404') && ['/cli/install.sh', '/cli/ow-linux-amd64'].some(path => message.location().url.endsWith(path)))) errors.push(message.text()); });
  page.on('pageerror', error => errors.push(error.message));
  await page.goto('/');
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
  await machines(page, 'quiet');
  await detailAction(page, 'Snapshot').click();
  await expect(dialog(page)).toBeVisible();
  await page.getByLabel('Snapshot name').fill('still-valid');
  // Browsers compile pattern attributes with the v flag; an invalid one is silently ignored.
  const invalid = await page.evaluate(() => [...document.querySelectorAll('[pattern]')].map(input => input.getAttribute('pattern')).filter(pattern => { try { new RegExp(pattern, 'v'); return false; } catch { return true; } }));
  expect(invalid).toEqual([]);
  expect(errors).toEqual([]);
});

test('an invalid name is refused in the browser and names the allowed characters', async ({dashboard: page}) => {
  await machines(page, 'named');
  const sent = [];
  page.on('request', request => { if (request.url().endsWith('/api/operation')) sent.push(request.postData()); });
  await detailAction(page, 'Snapshot').click();
  const name = page.getByLabel('Snapshot name');
  await name.fill('bad name!');
  await page.getByRole('button', {name: 'Capture snapshot'}).click();
  await expect(dialog(page)).toBeVisible();
  expect(await name.evaluate(input => input.validity.patternMismatch)).toBe(true);
  await expect(name).toHaveAttribute('title', 'Letters, digits, - and _, up to 32 characters.');
  await name.fill('good-name_1');
  expect(await name.evaluate(input => input.validity.valid)).toBe(true);
  expect(sent).toEqual([]);
});

test('a dialog unlocks with its result while the state reload is slow', async ({dashboard: page}) => {
  await machines(page, 'first', 'second');
  let release;
  const held = new Promise(resolve => { release = resolve; });
  await page.locator('.machine-card[data-machine="first"]').click();
  await page.route('**/api/state', async route => { await held; await route.continue().catch(() => {}); });
  await detailAction(page, 'Rename').click();
  await page.getByLabel('New machine name').fill('second');
  await page.getByRole('button', {name: 'Rename machine'}).click();
  // The refusal shows, and the dialog can be retried or closed, before /api/state answers.
  await expect(page.locator('#dialog-error')).toHaveText(/already have a machine named 'second'/);
  await expect(page.getByRole('button', {name: 'Rename machine'})).toBeEnabled();
  await expect(page.locator('#cancel-dialog')).toBeEnabled();
  await page.locator('#cancel-dialog').click();
  await expect(dialog(page)).toBeHidden();
  await expect(detailAction(page, 'Snapshot')).toBeEnabled();
  release();
  await page.unroute('**/api/state');
});

test('the snapshot dialog clears the last error and shows its progress', async ({dashboard: page}) => {
  await machines(page, 'slow');
  let release;
  const held = new Promise(resolve => { release = resolve; });
  let attempts = 0;
  await page.route('**/api/operation', async route => {
    if (++attempts === 1) return route.fulfill({status: 409, json: {ok: false, error: 'Not enough free memory to capture now.'}});
    await held;
    await route.continue();
  });
  await detailAction(page, 'Snapshot').click();
  await page.getByRole('button', {name: 'Capture snapshot'}).click();
  await expect(page.locator('#dialog-error')).toHaveText('Not enough free memory to capture now.');
  await page.getByRole('button', {name: 'Capture snapshot'}).click();
  await expect(page.locator('#dialog-error')).toBeHidden();
  await expect(page.locator('#dialog-submit')).toHaveText('Capturing…');
  await expect(page.locator('#dialog-submit')).toBeDisabled();
  release();
  await expect(notice(page)).toHaveText('Snapshot saved.');
  await expect(dialog(page)).toBeHidden();
  await expect(page.locator('#dialog-submit')).toHaveText('Capture snapshot');
});

test('a refused operation leaves no notice behind once its dialog is cancelled', async ({dashboard: page}) => {
  await machines(page, 'parent');
  await page.route('**/api/operation', route => route.fulfill({status: 409, json: {ok: false, error: 'insufficient host memory for 256 MiB'}}));
  await detailAction(page, 'Fork').click();
  await page.getByLabel('New machine name').fill('child');
  await page.getByRole('button', {name: 'Create fork'}).click();
  await expect(page.locator('#dialog-error')).toHaveText('insufficient host memory for 256 MiB');
  await page.getByRole('button', {name: 'Cancel'}).click();
  await expect(dialog(page)).toBeHidden();
  await expect(notice(page)).toBeHidden();
});

test('a gateway error says the server is not answering, not that the login expired', async ({dashboard: page}) => {
  await page.route('**/api/state', route => route.fulfill({status: 504, contentType: 'text/html', body: '<html>Gateway Timeout</html>'}));
  await page.getByRole('button', {name: '↻ Refresh'}).click();
  await expect(notice(page)).toHaveText('The server is not answering (HTTP 504); it may be busy or restarting. The page retries on its own.');
  await expect(notice(page)).toHaveClass(/error/);
  await page.unroute('**/api/state');
  // The periodic reload recovers without a manual refresh.
  await expect(page.locator('#updated')).toHaveText(/^Updated/, {timeout: 10_000});
  await expect(notice(page)).toBeHidden();
});

test('a result notice is cleared when the user switches view or machine', async ({dashboard: page}) => {
  await machines(page, 'one', 'two');
  await page.locator('#command').fill('true');
  await page.getByRole('button', {name: 'Run ↵'}).click();
  await expect(notice(page)).toHaveText('Command finished · exit 0');
  await page.getByRole('button', {name: /Snapshots/}).click();
  await expect(notice(page)).toBeHidden();
  await page.getByRole('button', {name: /Machines/}).click();
  await page.locator('#command').fill('true');
  await page.getByRole('button', {name: 'Run ↵'}).click();
  await expect(notice(page)).toHaveText('Command finished · exit 0');
  const other = await page.locator('#selected-name').textContent() === 'one' ? 'two' : 'one';
  await page.locator(`.machine-card[data-machine="${other}"]`).click();
  await expect(notice(page)).toBeHidden();
});

test('a command result started elsewhere does not follow the user to another view or machine', async ({dashboard: page}) => {
  await machines(page, 'one', 'two');
  for (const leave of [() => page.getByRole('button', {name: /Snapshots/}).click(), async () => {
    const other = await page.locator('#selected-name').textContent() === 'one' ? 'two' : 'one';
    await page.locator(`.machine-card[data-machine="${other}"]`).click();
  }]) {
    let release;
    const held = new Promise(resolve => { release = resolve; });
    await page.route('**/api/operation', async route => { await held; await route.continue(); });
    await page.getByRole('button', {name: /Machines/}).click();
    await page.locator('#command').fill('true');
    await page.getByRole('button', {name: 'Run ↵'}).click();
    await expect(notice(page)).toHaveText('Running command…');
    await leave();
    await expect(notice(page)).toBeHidden();
    const answered = page.waitForResponse('**/api/operation');
    release();
    await answered;
    await page.unroute('**/api/operation');
    // `busy` is the page script's own flag; it drops only after the result was handled.
    await page.waitForFunction(() => !busy);
    await expect(notice(page)).toBeHidden();
  }
});

test('a gateway error on an operation asks to try again, since an open dialog does not poll', async ({dashboard: page}) => {
  await machines(page, 'busy');
  await page.route('**/api/operation', route => route.fulfill({status: 504, contentType: 'text/html', body: '<html>Gateway Timeout</html>'}));
  await detailAction(page, 'Snapshot').click();
  await page.getByRole('button', {name: 'Capture snapshot'}).click();
  await expect(page.locator('#dialog-error')).toHaveText('The server is not answering (HTTP 504); it may be busy or restarting. Wait a moment, then try again.');
});

for (const status of [401, 403]) {
  test(`HTTP ${status} asks to sign in even with a JSON response`, async ({dashboard: page}) => {
    await page.route('**/api/state', route => route.fulfill({status, json: {ok: false, error: 'Unauthorized'}}));
    await page.getByRole('button', {name: '↻ Refresh'}).click();
    await expect(notice(page)).toHaveText('Your login may have expired. Reload the page to sign in again.');
  });
}

test('an unexpected HTML response without an auth redirect does not ask to sign in', async ({dashboard: page}) => {
  await page.route('**/api/state', route => route.fulfill({status: 200, contentType: 'text/html', body: '<html>Maintenance</html>'}));
  await page.getByRole('button', {name: '↻ Refresh'}).click();
  await expect(notice(page)).toHaveText('Unexpected response from the server (HTTP 200). Reload the page.');
});

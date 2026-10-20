import {test, expect, api, machines, card, notice, signIn} from './fixtures.mjs';

test('worker outage shows an error and recovers on refresh', async ({dashboard: page}) => {
  await machines(page, 'steady');
  await page.route('**/api/state', route => route.fulfill({status: 503, json: {ok: false, error: 'Workspace worker unavailable. Try refreshing.'}}));
  await page.getByRole('button', {name: '↻ Refresh'}).click();
  await expect(notice(page)).toHaveText('Workspace worker unavailable. Try refreshing.');
  await expect(notice(page)).toHaveClass(/error/);
  await expect(page.locator('#updated')).toHaveText('Connection interrupted');
  // The last known machines stay listed while the worker is away.
  await expect(card(page, 'steady')).toBeVisible();
  await page.unroute('**/api/state');
  await page.getByRole('button', {name: '↻ Refresh'}).click();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
});

test('failed first load offers a retry and keeps New machine disabled', async ({page}) => {
  await page.route('**/api/state', route => route.fulfill({status: 503, json: {ok: false, error: 'Workspace worker unavailable. Try refreshing.'}}));
  await page.goto('/');
  await expect(page.locator('#machine-grid .empty')).toHaveText('Could not reach your machines. Retry');
  await expect(page.getByRole('button', {name: '＋ New machine'})).toBeDisabled();
  await expect(page.locator('#pool-policy')).not.toBeEmpty();
  await page.unroute('**/api/state');
  await page.locator('#machine-grid').getByRole('button', {name: 'Retry'}).click();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
  await expect(page.locator('#machine-grid .empty')).toHaveText(/Your first machine is one click away/);
  await expect(page.getByRole('button', {name: '＋ New machine'})).toBeEnabled();
});

test('expired login asks the user to reload and sign in', async ({page}) => {
  // An identity proxy redirects an expired session to its HTML login page.
  // Use the real HTML root as the redirect target; Playwright does not route subsequent redirect hops.
  await page.route('**/api/state', route => route.fulfill({status: 302, headers: {location: '/'}}));
  await page.goto('/');
  await expect(notice(page)).toHaveText('Your login may have expired. Reload the page to sign in again.');
  await expect(page.locator('#updated')).toHaveText('Connection interrupted');
});

test('the tailnet dashboard offers no CLI installer', async ({dashboard: page}) => {
  // Tailnet sign-in serves no /cli/install.sh, so the install panel hides itself.
  await expect(page.locator('#cli-install')).toBeHidden();
});

test('the tailnet dashboard hides the sidebar CLI download', async ({dashboard: page}) => {
  // Tailnet sign-in serves no CLI binary, so the link would download a 404 page.
  expect(await page.evaluate(async () => (await fetch('/cli/ow-linux-amd64', {method: 'HEAD'})).ok)).toBe(false);
  await expect(page.getByRole('link', {name: /Download CLI/})).toBeHidden();
});

test.describe('narrow width', () => {
  test.use({viewport: {width: 390, height: 844}, isMobile: true, hasTouch: true});

  test('phone layout has no horizontal overflow and dialogs fit', async ({dashboard: page}) => {
    await machines(page, 'pocket-machine-with-a-long-name');
    const overflow = () => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(await overflow()).toBeLessThanOrEqual(0);
    await expect(card(page, 'pocket-machine-with-a-long-name')).toBeVisible();
    await page.locator('#command').fill('echo a-very-long-output-line-that-should-wrap-or-scroll-inside-the-console-only');
    await page.getByRole('button', {name: 'Run ↵'}).click();
    await expect(notice(page)).toHaveText('Command finished · exit 0');
    expect(await overflow()).toBeLessThanOrEqual(0);
    await page.getByRole('button', {name: '＋ New machine'}).click();
    const box = await page.locator('#operation-dialog').boundingBox();
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(390);
    await expect(page.getByRole('button', {name: 'Create machine'})).toBeInViewport();
  });

  test('a long signed-in email does not widen the phone layout', async ({page, user}) => {
    // 64 characters is the longest local part an address may have.
    const name = user.padEnd(64, 'a');
    const email = `${name}@example.test`;
    await signIn(page.context(), name);
    await page.goto('/');
    await expect(page.locator('#account-email')).toHaveText(email);
    await expect(page.locator('#account-email')).toHaveAttribute('title', email);
    expect(await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)).toBeLessThanOrEqual(0);
  });
});

test('a new machine shows a neutral first recovery point', async ({dashboard: page}) => {
  await machines(page, 'fresh');
  const status = card(page, 'fresh').locator('.card-lineage', {hasText: 'recovery'});
  await expect(status).toHaveText('First recovery point pending');
  await expect(status).not.toHaveClass(/recovery-warning/);
});

test('recovery status warns only past the one-hour target and shows relative time', async ({dashboard: page}) => {
  await machines(page, 'late', 'saved');
  const now = Math.floor(Date.now() / 1000);
  // Machines three hours old with recovery reports the fake worker does not make.
  const state = await api(page);
  for (const m of state.workspaces) {
    m.created_at = now - 3 * 3600;
    m.durability = m.id === 'late' ? {last_durable_at: null, pending: 'r1', current_state_durable: false} : {last_durable_at: now - 12 * 60, current_state_durable: false, retention: 'kept'};
  }
  await page.route('**/api/state', route => route.fulfill({json: {ok: true, result: state}}));
  await page.getByRole('button', {name: '↻ Refresh'}).click();
  const late = card(page, 'late').locator('.card-lineage', {hasText: 'durable'});
  await expect(late).toHaveText('No durable recovery point yet · one-hour disk recovery target exceeded · upload pending');
  await expect(late).toHaveClass(/recovery-warning/);
  const saved = card(page, 'saved').locator('.card-lineage', {hasText: 'durable'});
  await expect(saved).toHaveText('Last durable 12 min ago · off-host copies retained');
  await expect(saved).not.toHaveClass(/recovery-warning/);
  await expect(saved).toHaveAttribute('title', /^Last durable .*\d.*\nkept$/);
});

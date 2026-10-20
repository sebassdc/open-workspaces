import {test, expect, machines, card, notice} from './fixtures.mjs';

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

test('expired login asks the user to reload and sign in', async ({page}) => {
  // An identity proxy answers an expired session with its HTML login page, not JSON.
  await page.route('**/api/state', route => route.fulfill({status: 200, contentType: 'text/html', body: '<html>sign in</html>'}));
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
});

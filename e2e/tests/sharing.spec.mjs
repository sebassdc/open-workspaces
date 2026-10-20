import {test, expect, machines, card, detailAction, notice, signIn} from './fixtures.mjs';

test('share dialog grants a teammate, who sees it under shared with me', async ({dashboard: page, browser, user}) => {
  await machines(page, 'together');
  const teammate = `${user}-mate`;
  await detailAction(page, 'Share').click();
  await expect(page.locator('#dialog-title')).toHaveText('Share this machine.');
  await expect(page.locator('#dialog-description')).toHaveText(/A fork they make copies everything/);
  await page.getByLabel('Teammate email').fill(`${teammate}@example.test`);
  await page.getByLabel('Role').selectOption('use');
  await page.getByRole('button', {name: 'Share machine'}).click();
  await expect(notice(page)).toHaveText('Machine shared.');
  await expect(page.locator('#shared-with')).toContainText(`Shared with ${teammate}@example.test · use`);

  const other = await browser.newContext({baseURL: 'https://dashboard.e2e.ts.net', ignoreHTTPSErrors: true});
  await signIn(other, teammate);
  const theirs = await other.newPage();
  await theirs.goto('/');
  await expect(theirs.locator('#shared-view')).toBeVisible();
  const shared = theirs.locator('#shared-grid .machine-card');
  await expect(shared).toContainText(`Shared by ${user}@example.test · use`);
  await shared.click();
  // A use grant offers the terminal and fork, but not owner or admin actions.
  await expect(detailAction(theirs, 'Open Terminal')).toBeVisible();
  for (const name of ['Stop', 'Snapshot', 'Share', 'Rename', 'Delete']) await expect(detailAction(theirs, name)).toHaveCount(0);

  await page.getByRole('button', {name: 'Unshare'}).click();
  await expect(notice(page)).toHaveText('Access removed; their open sessions were closed.');
  await expect(page.locator('#shared-with')).toBeHidden();
  await theirs.reload();
  await expect(theirs.locator('#shared-view')).toBeHidden();
  await other.close();
});

test('share dialog refuses an invalid email', async ({dashboard: page}) => {
  await machines(page, 'solo');
  await detailAction(page, 'Share').click();
  await page.getByLabel('Teammate email').fill('not-an-email');
  await page.getByRole('button', {name: 'Share machine'}).click();
  // The browser's own email validation keeps the dialog open; nothing is sent.
  await expect(page.locator('#operation-dialog')).toBeVisible();
  expect(await page.getByLabel('Teammate email').evaluate(input => input.validity.valid)).toBe(false);
  await expect(card(page, 'solo')).toBeVisible();
});

test('share dialog refuses your own login plainly', async ({dashboard: page, user}) => {
  await machines(page, 'mine');
  await detailAction(page, 'Share').click();
  await page.getByLabel('Teammate email').fill(`${user}@example.test`);
  await page.getByRole('button', {name: 'Share machine'}).click();
  await expect(page.locator('#dialog-error')).toHaveText('You already own this machine; share it with a teammate login.');
});

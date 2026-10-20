import {test, expect, machines, card, detailAction, notice, signIn, deleteMachines} from './fixtures.mjs';

// `pooladmin` is the harness's configured pool admin; every other test user is not.
async function as(browser, name) {
  const context = await browser.newContext({baseURL: 'https://dashboard.e2e.ts.net', ignoreHTTPSErrors: true});
  await signIn(context, name);
  const page = await context.newPage();
  await page.goto('/');
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
  return {context, page};
}

test('transfer dialog gives a machine to another owner, and both see the change', async ({dashboard: page, browser, user}) => {
  await machines(page, 'handover');
  const next = `${user}-next`;
  // Not a pool admin: the create dialog offers no owner.
  await page.locator('#create').click();
  await expect(page.getByLabel('Owner email (empty: you)')).toHaveCount(0);
  await page.locator('#cancel-dialog').click();
  await detailAction(page, 'Transfer').click();
  await expect(page.locator('#dialog-title')).toHaveText('Give this machine away.');
  await page.getByLabel('New owner email').fill(`${next}@example.test`);
  await expect(page.getByLabel('Old owner')).toHaveValue('false');
  await page.getByRole('button', {name: 'Transfer machine'}).click();
  await expect(notice(page)).toHaveText(`Machine transferred to ${next}@example.test.`);
  await expect(card(page, 'handover')).toHaveCount(0);
  await expect(page.locator('#audit-list')).toContainText(`${user}@example.test transferred handover from ${user}@example.test to ${next}@example.test`);

  const {context, page: theirs} = await as(browser, next);
  await expect(card(theirs, 'handover')).toBeVisible();
  await expect(card(theirs, 'handover')).toContainText('running');
  await expect(theirs.locator('#audit-list')).toContainText(`transferred handover from ${user}@example.test to ${next}@example.test`);
  await deleteMachines(context);
  await context.close();
});

test('pool admin creates a machine for someone, who forks it for a teammate', async ({dashboard: page, browser, user}) => {
  const {context, page: admin} = await as(browser, 'pooladmin');
  const name = `${user}-gift`.slice(0, 32);
  await admin.locator('#create').click();
  await admin.getByLabel('Machine name').fill(name);
  await admin.getByLabel('Owner email (empty: you)').fill(`${user}@example.test`);
  await admin.getByRole('button', {name: 'Create machine'}).click();
  await expect(notice(admin)).toHaveText(`Machine created for ${user}@example.test.`);
  await expect(card(admin, name)).toHaveCount(0);
  await context.close();

  await page.reload();
  await expect(card(page, name)).toBeVisible();
  await expect(page.locator('#audit-list')).toContainText(`pooladmin@example.test created ${name} for ${user}@example.test`);
  await card(page, name).click();
  await detailAction(page, 'Fork').click();
  await page.getByLabel('New machine name').fill('for-mate');
  await page.getByLabel('Owner email (empty: you)').fill(`${user}-mate@example.test`);
  await page.getByRole('button', {name: 'Create fork'}).click();
  await expect(notice(page)).toHaveText(`Fork created for ${user}-mate@example.test.`);
  await expect(card(page, 'for-mate')).toHaveCount(0);
  await expect(page.locator('#audit-list')).toContainText(`${user}@example.test forked ${user}@example.test/${name} into ${user}-mate@example.test/for-mate`);

  const mate = await as(browser, `${user}-mate`);
  await expect(card(mate.page, 'for-mate')).toBeVisible();
  await deleteMachines(mate.context);
  await mate.context.close();
});

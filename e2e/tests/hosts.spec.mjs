import {test, expect, signIn, notice} from './fixtures.mjs';

test('host pool owner invites, budgets and revokes a Linux host', async ({context, page}) => {
  // Only the configured pool owner gets host controls.
  await signIn(context, 'owner');
  await page.goto('/');
  await expect(page.locator('#host-controls')).toBeVisible();
  await expect(page.locator('#pool-policy')).toHaveText(/Trusted shared private pool/);

  await page.getByRole('button', {name: 'Invite Linux host'}).click();
  const form = page.locator('#host-dialog');
  await expect(form.locator('#host-title')).toHaveText('Invite a trusted Linux host');
  await form.getByLabel('Node label').fill('e2e-node');
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

import {test, expect, api, machines, card, detailAction, dialog, notice} from './fixtures.mjs';

test('hibernate and resume keep the machine', async ({dashboard: page}) => {
  await machines(page, 'sleeper');
  await detailAction(page, 'Hibernate').click();
  await expect(notice(page)).toHaveText('Machine hibernated. Memory and disk state saved.');
  await expect(card(page, 'sleeper')).toContainText('hibernated');
  await expect(page.locator('#command')).toBeDisabled();
  await detailAction(page, 'Resume machine').click();
  await expect(notice(page)).toHaveText('Machine started.');
  await expect(card(page, 'sleeper')).toContainText('running');
  await expect(page.locator('#command')).toBeEnabled();
});

test('stop explains what is lost, then start brings it back', async ({dashboard: page}) => {
  await machines(page, 'halt');
  await detailAction(page, 'Stop').click();
  await expect(page.locator('#dialog-title')).toHaveText('Stop this machine?');
  await expect(page.locator('#dialog-description')).toHaveText(/Running processes and memory state are lost/);
  await page.getByRole('button', {name: 'Stop machine'}).click();
  await expect(notice(page)).toHaveText('Machine stopped. Disk changes retained.');
  await expect(card(page, 'halt')).toContainText('stopped');
  await detailAction(page, 'Start machine').click();
  await expect(notice(page)).toHaveText('Machine started.');
  await expect(card(page, 'halt')).toContainText('running');
});

test('resize grows a stopped machine disk', async ({dashboard: page}) => {
  await machines(page, 'grow');
  // Only a stopped machine offers Resize.
  await expect(detailAction(page, 'Resize')).toHaveCount(0);
  await api(page, {op: 'stop', id: 'grow'});
  await page.reload();
  await detailAction(page, 'Resize').click();
  const disk = page.getByLabel('Root disk (GiB)');
  await expect(disk).toHaveValue('32');
  await expect(disk).toHaveAttribute('min', '32');
  // The current size changes nothing, so the dialog says so instead of resizing.
  await page.getByRole('button', {name: 'Resize machine'}).click();
  await expect(page.locator('#dialog-error')).toHaveText('Already 32 GiB. Choose a larger size to grow it.');
  await expect(dialog(page)).toBeVisible();
  await expect(notice(page)).toBeHidden();
  await disk.fill('64');
  await page.getByRole('button', {name: 'Resize machine'}).click();
  await expect(notice(page)).toHaveText('Machine resized. Start it to grow the filesystem.');
  await expect(card(page, 'grow')).toContainText('64.0 GiB disk');
});

test('delete warns about snapshots and removes the machine', async ({dashboard: page}) => {
  await machines(page, 'doomed', 'keeper');
  await api(page, {op: 'snapshot', id: 'doomed', name: 'last-look'});
  await page.reload();
  await card(page, 'doomed').click();
  // A running machine cannot be deleted from the dashboard.
  await expect(detailAction(page, 'Delete')).toHaveCount(0);
  await api(page, {op: 'stop', id: 'doomed'});
  await page.reload();
  await card(page, 'doomed').click();
  await detailAction(page, 'Delete').click();
  await expect(page.locator('#dialog-title')).toHaveText('Delete this machine?');
  await expect(page.locator('#dialog-description')).toHaveText(/removed for good/);
  await expect(page.locator('#dialog-fields .field-error')).toHaveText('Its 1 snapshot will be deleted too: last-look');
  // The dialog leads with the warning and its confirm looks destructive, unlike Create.
  await expect(page.locator('#dialog-eyebrow')).toHaveText('CANNOT BE UNDONE');
  await expect(page.locator('#dialog-submit')).toHaveClass('button danger');
  await expect(page.getByRole('button', {name: 'Cancel'})).toHaveClass('button secondary');
  await page.getByRole('button', {name: 'Delete machine'}).click();
  await expect(dialog(page)).toBeHidden();
  await expect(notice(page)).toHaveText('Machine deleted.');
  await expect(card(page, 'doomed')).toHaveCount(0);
  await expect(card(page, 'keeper')).toBeVisible();
  expect((await api(page)).snapshots).toEqual([]);
});

test('delete of a machine without snapshots says so', async ({dashboard: page}) => {
  await machines(page, 'bare');
  await api(page, {op: 'stop', id: 'bare'});
  await page.reload();
  await detailAction(page, 'Delete').click();
  await expect(page.locator('#dialog-fields')).toContainText('It has no snapshots.');
  await page.getByRole('button', {name: 'Cancel'}).click();
  await expect(card(page, 'bare')).toBeVisible();
});

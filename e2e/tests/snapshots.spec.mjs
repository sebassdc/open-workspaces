import {test, expect, api, machines, card, detailAction, notice} from './fixtures.mjs';

test('snapshot dialog saves a named checkpoint', async ({dashboard: page}) => {
  await machines(page, 'origin');
  await detailAction(page, 'Snapshot').click();
  await expect(page.locator('#dialog-title')).toHaveText('Save this moment.');
  await expect(page.getByLabel('Snapshot name')).toHaveValue(/^snapshot-/);
  await page.getByLabel('Snapshot name').fill('checkpoint-one');
  await page.getByRole('button', {name: 'Capture snapshot'}).click();
  await expect(notice(page)).toHaveText('Snapshot saved.');
  await page.getByRole('button', {name: /Snapshots/}).click();
  const row = page.locator('.snapshot-row', {hasText: 'checkpoint-one'});
  await expect(row).toContainText('origin · 256 MiB · memory + disk');
  await expect(row.getByRole('button', {name: 'Fork a machine'})).toBeVisible();
  await expect(row.getByRole('button', {name: 'Restore'})).toBeVisible();
});

test('fork from a running machine creates an independent child', async ({dashboard: page}) => {
  await machines(page, 'parent');
  await detailAction(page, 'Fork').click();
  await expect(page.locator('#dialog-title')).toHaveText('Try another direction.');
  await expect(page.getByLabel('Start from')).toHaveValue('');
  await page.getByLabel('New machine name').fill('child');
  await page.getByRole('button', {name: 'Create fork'}).click();
  await expect(notice(page)).toHaveText('Independent fork created.');
  await expect(card(page, 'child')).toContainText('Forked from parent');
  await expect(page.locator('#selected-name')).toHaveText('child');
});

test('fork and restore from the snapshot list', async ({dashboard: page}) => {
  await machines(page, 'base');
  await api(page, {op: 'snapshot', id: 'base', name: 'saved'});
  await page.reload();
  await page.getByRole('button', {name: /Snapshots/}).click();
  const row = page.locator('.snapshot-row', {hasText: 'saved'});
  await row.getByRole('button', {name: 'Fork a machine'}).click();
  await expect(page.locator('#dialog-fields')).toContainText('Machine: base · Snapshot: saved');
  await page.getByLabel('New machine name').fill('branch');
  await page.getByRole('button', {name: 'Create fork'}).click();
  await expect(notice(page)).toHaveText('Independent fork created.');
  await row.getByRole('button', {name: 'Restore'}).click();
  await expect(page.locator('#dialog-title')).toHaveText('Return to this checkpoint?');
  await expect(page.locator('#dialog-description')).toHaveText(/Changes made since that snapshot will be lost/);
  await page.getByRole('button', {name: 'Restore snapshot'}).click();
  await expect(notice(page)).toHaveText('Snapshot restored.');
  await page.getByRole('button', {name: /Machines/}).click();
  await expect(card(page, 'branch')).toContainText('Forked from base');
});

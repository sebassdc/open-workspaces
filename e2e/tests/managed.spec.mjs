import {test, expect, api, machines, manage, card, detailAction, notice} from './fixtures.mjs';

// A moved machine (a committed placement, as a move between runners leaves it): every action
// reaches the runner with that placement, which the fake worker requires, as the real one does.
test('a moved machine snapshots, stops, resizes and deletes with its snapshots', async ({dashboard: page}) => {
  await machines(page, 'roamer');
  await manage(page, 'roamer');
  await page.reload();
  await card(page, 'roamer').click();
  await detailAction(page, 'Snapshot').click();
  await page.getByLabel('Snapshot name').fill('roam-saved');
  await page.getByRole('button', {name: 'Capture snapshot'}).click();
  await expect(notice(page)).toHaveText('Snapshot saved.');
  await detailAction(page, 'Stop').click();
  await page.getByRole('button', {name: 'Stop machine'}).click();
  await expect(notice(page)).toHaveText('Machine stopped. Disk changes retained.');
  await detailAction(page, 'Resize').click();
  await page.getByLabel('Root disk (GiB)').fill('64');
  await page.getByRole('button', {name: 'Resize machine'}).click();
  await expect(notice(page)).toHaveText('Machine resized. Start it to grow the filesystem.');
  await expect(card(page, 'roamer')).toContainText('64.0 GiB disk');
  await detailAction(page, 'Delete').click();
  await expect(page.locator('#dialog-fields .field-error')).toHaveText('Its 1 snapshot will be deleted too: roam-saved');
  await page.getByRole('button', {name: 'Delete machine'}).click();
  await expect(notice(page)).toHaveText('Machine deleted.');
  await expect(card(page, 'roamer')).toHaveCount(0);
  expect((await api(page)).snapshots).toEqual([]);
});

test('a moved machine resizes its memory and vCPUs while stopped', async ({dashboard: page}) => {
  await machines(page, 'drifter');
  await manage(page, 'drifter');
  await api(page, {op: 'stop', id: 'drifter'});
  const resized = await api(page, {op: 'resize', id: 'drifter', memory_mib: 512, vcpu_count: 2});
  expect([resized.memory_mib, resized.vcpu_count]).toEqual([512, 2]);
  await page.reload();
  await expect(card(page, 'drifter')).toContainText('2 vCPU');
  await expect(card(page, 'drifter')).toContainText('512 MiB');
});

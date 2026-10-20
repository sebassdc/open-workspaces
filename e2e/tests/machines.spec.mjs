import {test, expect, api, machines, card, detailAction, dialog, notice, signIn} from './fixtures.mjs';

test('machine list shows cards, counts and the selected machine', async ({dashboard: page}) => {
  await machines(page, 'alpha', 'beta');
  await expect(page.locator('.machine-card')).toHaveCount(2);
  await expect(page.locator('#nav-count')).toHaveText('2');
  await expect(page.locator('#metric-running')).toHaveText('2');
  await expect(card(page, 'alpha')).toContainText('running');
  await expect(card(page, 'alpha')).toContainText('256 MiB');
  await expect(card(page, 'alpha')).toContainText('32.0 GiB disk');
  // The first machine is selected; clicking another selects it instead.
  await expect(card(page, 'alpha')).toHaveAttribute('aria-pressed', 'true');
  await card(page, 'beta').click();
  await expect(card(page, 'beta')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('#selected-name')).toHaveText('beta');
});

test('empty sandbox invites a first machine', async ({dashboard: page}) => {
  await expect(page.locator('#machine-grid .empty')).toHaveText(/Your first machine is one click away/);
  await expect(page.locator('#nav-count')).toHaveText('0');
  await expect(page.locator('#detail')).toBeHidden();
  await page.getByRole('button', {name: /Snapshots/}).click();
  await expect(page.locator('#snapshot-list .empty')).toHaveText(/No checkpoints yet/);
});

test('memory counter shows your usage against your owner limits', async ({dashboard: page}) => {
  await expect(page.locator('#metric-reserved')).toHaveText('0 MiB of 32.0 GiB');
  await machines(page, 'counted');
  // The harness uses the default owner caps: 8 machines, 16 vCPU, 32 GiB memory, 256 GiB disk.
  await expect(page.locator('#metric-reserved')).toHaveText('256 MiB of 32.0 GiB');
  await expect(page.locator('#metric-limit')).toHaveText('1 of 8 machines · 1 of 16 vCPU · 32 of 256 GiB disk');
});

test('create dialog creates a machine with the chosen shape', async ({dashboard: page}) => {
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await expect(dialog(page)).toBeVisible();
  await expect(page.locator('#dialog-title')).toHaveText('A fresh place to build.');
  await expect(page.locator('#dialog-eyebrow')).toHaveText('YOUR SANDBOX');
  await expect(page.locator('#dialog-submit')).toHaveClass('button primary');
  await page.getByLabel('Machine name').fill('made-in-ui');
  await page.getByLabel('Operating system').selectOption('ubuntu');
  await page.getByLabel('Memory').selectOption('512');
  await page.locator('select[name=vcpu_count]').selectOption('2');
  await page.getByLabel('Root disk (GiB)').fill('40');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(dialog(page)).toBeHidden();
  await expect(notice(page)).toHaveText('Machine created.');
  await expect(card(page, 'made-in-ui')).toContainText('2 vCPU');
  await expect(card(page, 'made-in-ui')).toContainText('512 MiB');
  await expect(card(page, 'made-in-ui')).toContainText('40.0 GiB disk');
  await expect(card(page, 'made-in-ui')).toContainText('Ubuntu 24.04');
  await expect(page.locator('#selected-name')).toHaveText('made-in-ui');
});

test('create dialog shows a refused create and stays open', async ({dashboard: page}) => {
  await page.getByRole('button', {name: '＋ New machine'}).click();
  await page.getByLabel('Machine name').fill('too-big');
  // 16 GiB is more than this host's 4 GiB of capacity.
  await page.getByLabel('Memory').selectOption('16384');
  await page.getByRole('button', {name: 'Create machine'}).click();
  await expect(page.locator('#dialog-error')).toBeVisible();
  await expect(page.locator('#dialog-error')).not.toBeEmpty();
  await expect(dialog(page)).toBeVisible();
  await expect(notice(page)).toHaveClass(/error/);
  await page.getByRole('button', {name: 'Cancel'}).click();
  await expect(dialog(page)).toBeHidden();
  await expect(card(page, 'too-big')).toBeVisible();
});

test('rename keeps the machine selected under its new name', async ({dashboard: page}) => {
  await machines(page, 'old-name');
  await page.locator('#command').fill('echo before-rename');
  await page.getByRole('button', {name: 'Run ↵'}).click();
  await expect(page.locator('#console-output')).toContainText('before-rename');
  await detailAction(page, 'Rename').click();
  await expect(page.locator('#dialog-description')).toHaveText(/Only the name changes/);
  await page.getByLabel('New machine name').fill('new-name');
  await page.getByRole('button', {name: 'Rename machine'}).click();
  await expect(notice(page)).toHaveText('Machine renamed.');
  await expect(card(page, 'new-name')).toBeVisible();
  await expect(card(page, 'old-name')).toHaveCount(0);
  await expect(page.locator('#selected-name')).toHaveText('new-name');
  // The console history follows the machine.
  await expect(page.locator('#console-output')).toContainText('before-rename');
  expect((await api(page)).workspaces.map(m => m.id)).toEqual(['new-name']);
});

test('navigation switches between machines, snapshots and usage', async ({dashboard: page}) => {
  await machines(page, 'metered');
  await page.getByRole('button', {name: /Resource usage/}).click();
  await expect(page.locator('#page-title')).toHaveText('Room to grow.');
  await expect(page.locator('#breadcrumb')).toHaveText('Your sandbox / Resource usage');
  await expect(page.locator('#usage-view')).toBeVisible();
  await expect(page.locator('#machines-view')).toBeHidden();
  await expect(page.locator('#capacity-text')).toHaveText(/\/ 4\.0 GiB$/);
  await page.getByRole('button', {name: /Snapshots/}).click();
  await expect(page.locator('#page-title')).toHaveText('Pick up from here.');
  await page.getByRole('button', {name: /Machines/}).click();
  await expect(page.locator('#machines-view')).toBeVisible();
});

test('top bar shows the signed-in account', async ({page, user}) => {
  // A name not starting with "u", so the avatar differs from the "U" placeholder; it creates no machines.
  const name = `z${user}`;
  await signIn(page.context(), name);
  await page.goto('/');
  await expect(page.locator('#account-email')).toHaveText(`${name}@example.test`);
  await expect(page.locator('#account-avatar')).toHaveText('Z');
});

test('cards keep their content at the top, whatever their height', async ({dashboard: page}) => {
  await machines(page, 'short', 'tall');
  // A taller neighbour in the same row stretches every card; content must not drift down.
  await card(page, 'tall').evaluate(node => node.append(Object.assign(document.createElement('span'), {className: 'card-lineage', textContent: 'extra line'})));
  const offsets = await page.locator('.machine-card').evaluateAll(cards => cards.map(c => c.querySelector('.machine-icon').getBoundingClientRect().top - c.getBoundingClientRect().top));
  expect(offsets).toHaveLength(2);
  // 1 px border + 21 px padding: the icon sits at the top, not centred.
  for (const offset of offsets) expect(offset).toBeCloseTo(22, 0);
  // The notice keeps a gap from the card below it.
  await card(page, 'short').click();
  await detailAction(page, 'Hibernate').click();
  await expect(notice(page)).toBeVisible();
  const gap = await notice(page).evaluate(node => {
    let next = node.nextElementSibling; while (next.hidden) next = next.nextElementSibling;
    return next.getBoundingClientRect().top - node.getBoundingClientRect().bottom;
  });
  expect(gap).toBeGreaterThanOrEqual(16);
});

test('on a phone, tapping a card brings its actions into view', async ({dashboard: page}) => {
  await page.setViewportSize({width: 390, height: 844});
  await machines(page, 'm1', 'm2', 'm3', 'm4', 'm5');
  await card(page, 'm1').evaluate(node => node.scrollIntoView({block: 'start'}));
  await expect(page.locator('.detail-heading')).not.toBeInViewport();
  await card(page, 'm1').click();
  await expect(page.locator('#selected-name')).toHaveText('m1');
  await expect(page.locator('.detail-heading')).toBeInViewport();
  await expect(detailAction(page, 'Hibernate')).toBeInViewport();
});

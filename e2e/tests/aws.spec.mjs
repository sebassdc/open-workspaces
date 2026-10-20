import {test, expect, api, machines, detailAction, notice, signIn} from './fixtures.mjs';

const ROLE = 'arn:aws:iam::111122223333:role/open-workspaces/e2e-reader';

test('aws role dialog attaches an allowed role, shows its token and detaches it', async ({dashboard: page}) => {
  await machines(page, 'cloud');
  await detailAction(page, 'AWS role').click();
  await expect(page.locator('#dialog-title')).toHaveText('Give this machine AWS access.');
  await expect(page.locator('#dialog-description')).toHaveText(/no AWS key is stored in it/);
  // Only the operator's allow-list is offered.
  await expect(page.getByLabel('IAM role').locator('option')).toHaveText([ROLE]);
  await page.getByRole('button', {name: 'Attach role'}).click();
  await expect(notice(page)).toHaveText('AWS role attached.');
  await expect(page.locator('#aws-role')).toContainText(`AWS role ${ROLE} · token active until`);
  const attached = (await api(page)).workspaces.find(m => m.id === 'cloud').aws_role;
  expect(attached.role_arn).toBe(ROLE);
  expect(attached.state).toBe('active');
  expect(attached.token_expires_at - Date.now() / 1000).toBeLessThanOrEqual(900);

  await page.locator('#aws-role').getByRole('button', {name: 'Detach'}).click();
  await expect(notice(page)).toHaveText('AWS role detached; its token was removed.');
  await expect(page.locator('#aws-role')).toBeHidden();
});

test('aws role is refused off the allow-list, not inherited by a fork, and not offered to a use grantee', async ({dashboard: page, browser, user}) => {
  await machines(page, 'parent');
  await expect(api(page, {op: 'aws-role', id: 'parent', role_arn: 'arn:aws:iam::111122223333:role/admin'})).rejects.toThrow(/not on this server's allow-list/);
  await api(page, {op: 'aws-role', id: 'parent', role_arn: ROLE});
  await api(page, {op: 'fork', id: 'parent', child: 'child'});
  const state = await api(page);
  expect(state.workspaces.find(m => m.id === 'parent').aws_role.role_arn).toBe(ROLE);
  expect(state.workspaces.find(m => m.id === 'child').aws_role).toBeNull();

  const teammate = `${user}-mate`;
  await api(page, {op: 'share', id: 'parent', with: `${teammate}@example.test`, role: 'use'});
  const other = await browser.newContext({baseURL: 'https://dashboard.e2e.ts.net', ignoreHTTPSErrors: true});
  await signIn(other, teammate);
  const theirs = await other.newPage();
  await theirs.goto('/');
  await theirs.locator('#shared-grid .machine-card').click();
  await expect(detailAction(theirs, 'Open Terminal')).toBeVisible();
  await expect(detailAction(theirs, 'AWS role')).toHaveCount(0);
  await expect(api(theirs, {op: 'aws-role', id: `${user}@example.test/parent`})).rejects.toThrow();
  await other.close();
});

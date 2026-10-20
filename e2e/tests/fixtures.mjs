import {test as base, expect} from '@playwright/test';

// Each test signs in as its own tailnet user (see harness/start.mjs), so it starts
// with an empty sandbox; its machines are stopped and deleted afterwards.
export const test = base.extend({
  user: [async ({context}, use, info) => {
    const name = `u${info.workerIndex}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
    await signIn(context, name);
    await use(name);
    await deleteMachines(context);
  }, {auto: true}],
  // The dashboard page with its first state load finished.
  dashboard: async ({page}, use) => {
    await page.goto('/');
    await expect(page.locator('#updated')).toHaveText(/^Updated/);
    await use(page);
  },
});
export {expect};

// Stops and deletes the signed-in user's machines.
export async function deleteMachines(context) {
  const page = await context.newPage();
  await page.goto('/');
  const state = await api(page);
  // One machine that will not go must not keep the others running for later tests.
  for (const machine of state.workspaces) {
    try {
      if (machine.state === 'running') await api(page, {op: 'stop', id: machine.id});
      await api(page, {op: 'delete', id: machine.id});
    } catch (error) { console.warn(`cleanup of ${machine.id}: ${error.message}`); }
  }
}

export async function signIn(context, name) {
  await context.addCookies([{name: 'ow-e2e-user', value: name, url: 'https://dashboard.e2e.ts.net'}]);
}

// Same-origin API call from inside the page, as the dashboard script makes it.
export async function api(page, body) {
  return page.evaluate(async body => {
    const response = await fetch(body ? '/api/operation' : '/api/state', body ? {method: 'POST', headers: {'content-type': 'application/json', 'x-ow-request': 'dashboard'}, body: JSON.stringify(body)} : {});
    const value = await response.json();
    if (!value.ok) throw Error(`${response.status} ${value.error}`);
    return value.result;
  }, body);
}

// Creates machines through the API, then reloads so the page lists them.
export async function machines(page, ...ids) {
  for (const id of ids) await api(page, {op: 'create', id});
  await page.reload();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
}

export const card = (page, id) => page.locator(`.machine-card[data-machine="${id}"]`);
export const detailAction = (page, name) => page.locator('#detail-actions').getByRole('button', {name, exact: true});
export const dialog = page => page.locator('#operation-dialog');
export const notice = page => page.locator('#notice');

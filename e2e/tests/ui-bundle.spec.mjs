import {test, expect} from './fixtures.mjs';

// Dashboard UI bundles (docs/DASHBOARD.md#ui-bundles) on the running controller. The
// harness installs and selects bundles as an operator would (`ow dashboard-ui install`,
// `ow config set dashboard.ui_bundle`) and reports the dashboard's PID, which must not change.
test.describe.configure({mode: 'serial'});

async function harness(page, step) {
  return page.evaluate(async step => {
    const response = await fetch('/__e2e/ui', {method: 'POST', body: JSON.stringify(step)});
    if (!response.ok) throw Error(`harness ${step.action}: ${response.status} ${await response.text()}`);
    return response.json();
  }, step);
}
const version = page => page.evaluate(async () => (await (await fetch('/api/version')).json()).result);

async function reload(page) {
  await page.reload();
  await expect(page.locator('#updated')).toHaveText(/^Updated/);
}

let initial;
test.beforeAll(async ({browser}) => {
  const page = await browser.newPage();
  await page.goto('/');
  initial = (await version(page)).ui;
  await page.close();
});
// Leave the selection the suite started with, whatever a test did.
test.afterAll(async ({browser}) => {
  const page = await browser.newPage();
  await page.goto('/');
  await harness(page, {action: 'select', version: initial.source === 'bundle' ? initial.version : 'builtin'});
  await page.close();
});

test('the API reports its version, the UI it serves and the bundle capability', async ({dashboard: page}) => {
  const v = await version(page);
  expect(v.api).toBe(1);
  expect(v.capabilities).toContain('dashboard_ui_bundle_v1');
  // CI runs the whole suite once against the built-in UI and once against a bundle.
  expect(v.ui.source).toBe(process.env.OW_E2E_UI === 'bundle' ? 'bundle' : 'builtin');
  expect(v.ui.fallback_reason).toBeNull();
  await expect(page.locator('#ui-version')).toHaveText(`Dashboard UI ${v.ui.version}${v.ui.source === 'builtin' ? ' (built-in)' : ''} · API 1`);
  await expect(page.locator('#ui-fallback')).toBeHidden();
});

test('a UI bundle switches live and rolls back without restarting the controller', async ({dashboard: page}) => {
  const {pid} = await harness(page, {action: 'pid'});
  for (const v of ['e2e-2', 'e2e-3']) expect((await harness(page, {action: 'install', version: v, marker: `${v} · `})).code).toBe(0);

  expect((await harness(page, {action: 'select', version: 'e2e-2'})).code).toBe(0);
  await reload(page);
  await expect(page).toHaveTitle('e2e-2 · Open Workspaces');
  await expect(page.locator('#ui-version')).toHaveText('Dashboard UI e2e-2 · API 1');

  expect((await harness(page, {action: 'select', version: 'e2e-3'})).code).toBe(0);
  await reload(page);
  await expect(page).toHaveTitle('e2e-3 · Open Workspaces');

  // Roll back the same way: to the previous bundle, then to the built-in copy.
  expect((await harness(page, {action: 'select', version: 'e2e-2'})).code).toBe(0);
  await reload(page);
  await expect(page).toHaveTitle('e2e-2 · Open Workspaces');
  expect((await harness(page, {action: 'select', version: 'builtin'})).code).toBe(0);
  await reload(page);
  await expect(page).toHaveTitle('Open Workspaces');
  await expect(page.locator('#ui-version')).toHaveText(/\(built-in\) · API 1$/);

  expect((await harness(page, {action: 'pid'})).pid).toBe(pid);
});

test('a damaged bundle falls back to the built-in UI and says why', async ({dashboard: page}) => {
  const {pid} = await harness(page, {action: 'pid'});
  expect((await harness(page, {action: 'install', version: 'e2e-4', marker: 'e2e-4 · '})).code).toBe(0);
  expect((await harness(page, {action: 'select', version: 'e2e-4'})).code).toBe(0);
  await reload(page);
  await expect(page).toHaveTitle('e2e-4 · Open Workspaces');

  await harness(page, {action: 'tamper', version: 'e2e-4'});
  await reload(page);
  await expect(page).toHaveTitle('Open Workspaces');
  await expect(page.locator('#ui-fallback')).toBeVisible();
  await expect(page.locator('#ui-fallback')).toContainText('app.js does not match its manifest hash');
  await expect(page.locator('#ui-version')).toHaveText(/\(built-in\) · API 1$/);
  // The fallback is a working dashboard, not an error page.
  await expect(page.locator('#machine-grid')).toBeVisible();

  // Selecting the damaged bundle again is refused.
  const refused = await harness(page, {action: 'select', version: 'e2e-4'});
  expect(refused.code).not.toBe(0);
  expect(refused.output).toContain('app.js does not match its manifest hash');
  expect((await harness(page, {action: 'pid'})).pid).toBe(pid);
});

test('a bundle for another dashboard API version is refused', async ({dashboard: page}) => {
  expect((await harness(page, {action: 'select', version: 'builtin'})).code).toBe(0);
  expect((await harness(page, {action: 'install', version: 'e2e-api2', api: [2, 2]})).code).toBe(0);
  const refused = await harness(page, {action: 'select', version: 'e2e-api2'});
  expect(refused.code).not.toBe(0);
  expect(refused.output).toContain('supports dashboard API 2-2; this server speaks 1');
  await reload(page);
  await expect(page).toHaveTitle('Open Workspaces');
  await expect(page.locator('#ui-fallback')).toBeHidden();
});

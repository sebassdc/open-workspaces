import {test, expect, machines, card, detailAction} from './fixtures.mjs';

// The dashboard refreshes every 5 s and rebuilds its lists; keyboard focus must survive it.
// Waits until the periodic refresh has replaced the given control with a new node.
async function nextRefresh(locator) {
  const old = await locator.elementHandle();
  await expect.poll(() => old.evaluate(node => node.isConnected), {timeout: 8_000}).toBe(false);
}

test('focus stays on an action button and a machine card across a refresh', async ({dashboard: page}) => {
  await machines(page, 'keyboard', 'other');
  const terminal = detailAction(page, 'Open Terminal');
  await terminal.focus();
  await nextRefresh(terminal);
  await expect(terminal).toBeFocused();

  await card(page, 'other').focus();
  await nextRefresh(card(page, 'other'));
  await expect(card(page, 'other')).toBeFocused();
  // Tab continues from the focused card, not from the top of the page.
  await page.keyboard.press('Shift+Tab');
  await expect(card(page, 'keyboard')).toBeFocused();
});

// Previews need `tailscale serve`, so this answers the published ports itself (as previews.spec does).
test('focus stays on the same port Unpublish when an earlier port goes away', async ({dashboard: page}) => {
  await machines(page, 'ports');
  let ports = [3000, 4000, 5000];
  await page.route('**/api/state', async route => {
    const response = await route.fetch({url: `https://127.0.0.1:${process.env.OW_E2E_PORT || 18443}/api/state`, headers: {...await route.request().allHeaders(), host: 'dashboard.e2e.ts.net'}});
    const body = await response.json();
    body.result.previews = true;
    for (const machine of body.result.workspaces) machine.published_ports = ports.map(port => ({port, https_port: port + 8000, url: `https://dashboard.e2e.ts.net:${port + 8000}/`}));
    await route.fulfill({response, json: body});
  });
  await page.reload();
  const unpublish = port => page.locator('#published-ports > span').filter({hasText: `Port ${port}`}).getByRole('button', {name: 'Unpublish'});
  await unpublish(4000).focus();
  ports = [4000, 5000];
  await nextRefresh(unpublish(4000));
  await expect(page.locator('#published-ports')).not.toContainText('Port 3000');
  await expect(unpublish(4000)).toBeFocused();
});

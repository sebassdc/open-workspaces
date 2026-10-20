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

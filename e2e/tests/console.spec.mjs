import {test, expect, api, machines, detailAction, notice} from './fixtures.mjs';

test('terminal opens, echoes input and reports the shell exit', async ({dashboard: page}) => {
  await machines(page, 'tty');
  await detailAction(page, 'Open Terminal').click();
  const terminal = page.locator('#terminal-dialog');
  await expect(terminal).toBeVisible();
  await expect(page.locator('#terminal-title')).toHaveText('tty / Terminal');
  await expect(page.locator('#terminal-status')).toHaveText('Connected');
  await expect(terminal.getByRole('button', {name: 'New session'})).toBeHidden();
  await expect(terminal.locator('.xterm-rows')).toContainText('fake-shell$');
  // The dark dialog asks for dark native scrollbars on the xterm viewport.
  expect(await terminal.locator('.xterm-viewport').evaluate(node => getComputedStyle(node).colorScheme)).toBe('dark');
  await terminal.locator('.xterm').click();
  await page.keyboard.type('hello-from-e2e');
  await page.keyboard.press('Enter');
  // The fake shell echoes the typed line, then prints it again as its output.
  await expect(terminal.locator('.xterm-rows')).toContainText(/hello-from-e2e[\s\S]*hello-from-e2e/);
  await page.keyboard.type('exit 3');
  await page.keyboard.press('Enter');
  await expect(page.locator('#terminal-status')).toHaveText('Shell exited · 3');
  await expect(terminal.getByRole('button', {name: 'Disconnect'})).toBeHidden();
  await expect(terminal.getByRole('button', {name: 'Close'})).toBeVisible();
  // A new session reconnects in the same dialog.
  await terminal.getByRole('button', {name: 'New session'}).click();
  await expect(page.locator('#terminal-status')).toHaveText('Connected');
  await expect(terminal.locator('.xterm-rows')).toContainText('fake-shell$');
  await expect(terminal.locator('.xterm-rows')).not.toContainText('hello-from-e2e');
  await terminal.getByRole('button', {name: 'Disconnect'}).click();
  await expect(terminal).toBeHidden();
});

test('terminal notices a machine hibernated elsewhere and blocks input', async ({dashboard: page}) => {
  await machines(page, 'napper');
  await detailAction(page, 'Open Terminal').click();
  const terminal = page.locator('#terminal-dialog'), rows = terminal.locator('.xterm-rows');
  await expect(page.locator('#terminal-status')).toHaveText('Connected');
  await expect(rows).toContainText('fake-shell$');
  // Another client hibernates it; the fake worker keeps the terminal stream open, as a frozen guest would.
  await api(page, {op: 'hibernate', id: 'napper'});
  await expect(page.locator('#terminal-status')).toHaveText('Machine hibernated. Start it, then choose New session.', {timeout: 10_000});
  await expect(terminal.getByRole('button', {name: 'Close'})).toBeVisible();
  await expect(terminal.getByRole('button', {name: 'New session'})).toBeVisible();
  await terminal.locator('.xterm').click();
  await page.keyboard.type('lost-input');
  await expect(rows).not.toContainText('lost-input');
  await terminal.getByRole('button', {name: 'Close'}).click();
  await expect(terminal).toBeHidden();
});

test('terminal explains a machine stopped elsewhere after its connection drops', async ({dashboard: page}) => {
  await machines(page, 'halted');
  await detailAction(page, 'Open Terminal').click();
  const terminal = page.locator('#terminal-dialog'), status = page.locator('#terminal-status');
  await expect(status).toHaveText('Connected');
  await expect(terminal.locator('.xterm-rows')).toContainText('fake-shell$');
  // The fake worker drops the stream at once, so the dialog first sees the close, then the poll.
  await api(page, {op: 'stop', id: 'halted'});
  await expect(status).toHaveText('Machine stopped. Start it, then choose New session.', {timeout: 10_000});
  await expect(terminal.getByRole('button', {name: 'New session'})).toBeVisible();
  await terminal.locator('.xterm').click();
  await page.keyboard.type('lost-input');
  await expect(terminal.locator('.xterm-rows')).not.toContainText('lost-input');
  await terminal.getByRole('button', {name: 'Close'}).click();
  await expect(terminal).toBeHidden();
});

test('command console shows output and exit status as text', async ({dashboard: page}) => {
  await machines(page, 'shell');
  const output = page.locator('#console-output');
  await expect(output).toHaveText(/Ready\. Run a command/);
  await page.locator('#command').fill('echo <b>not bold</b>');
  await page.locator('#command').press('Enter');
  await expect(notice(page)).toHaveText('Command finished · exit 0');
  await expect(output).toContainText('$ echo <b>not bold</b>\n<b>not bold</b>\n[exit 0]');
  await expect(output.locator('b')).toHaveCount(0);
  await page.locator('#command').fill('false');
  await page.getByRole('button', {name: 'Run ↵'}).click();
  await expect(notice(page)).toHaveText('Command finished · exit 1');
  await expect(notice(page)).toHaveClass(/error/);
  await page.getByRole('button', {name: 'Clear'}).click();
  await expect(output).toHaveText(/Ready\. Run a command/);
});

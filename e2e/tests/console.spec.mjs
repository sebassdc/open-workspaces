import {test, expect, machines, detailAction, notice} from './fixtures.mjs';

test('terminal opens, echoes input and reports the shell exit', async ({dashboard: page}) => {
  await machines(page, 'tty');
  await detailAction(page, 'Open Terminal').click();
  const terminal = page.locator('#terminal-dialog');
  await expect(terminal).toBeVisible();
  await expect(page.locator('#terminal-title')).toHaveText('tty / Terminal');
  await expect(page.locator('#terminal-status')).toHaveText('Connected');
  await expect(terminal.locator('.xterm-rows')).toContainText('fake-shell$');
  await terminal.locator('.xterm').click();
  await page.keyboard.type('hello-from-e2e');
  await page.keyboard.press('Enter');
  // The fake shell echoes the typed line, then prints it again as its output.
  await expect(terminal.locator('.xterm-rows')).toContainText(/hello-from-e2e[\s\S]*hello-from-e2e/);
  await page.keyboard.type('exit');
  await page.keyboard.press('Enter');
  await expect(page.locator('#terminal-status')).toHaveText('Shell exited · 0');
  await page.getByRole('button', {name: 'Disconnect'}).click();
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

test('a command past the 30 s limit shows a plain message, not the operation journal', async ({dashboard: page}) => {
  await machines(page, 'slow');
  await expect(page.locator('.console-note')).toContainText('up to 30 s');
  const output = page.locator('#console-output');
  await page.locator('#command').fill('sleep 34; echo leftover-from-slow');
  await page.locator('#command').press('Enter');
  await expect(output).toContainText('Stopped waiting after 30 s: commands in the console are limited to 30 s');
  await expect(output).toContainText('Use Open Terminal');
  await expect(output).not.toContainText('journal');
  await page.locator('#command').fill('echo next-command');
  await page.locator('#command').press('Enter');
  await expect(output).toContainText('$ echo next-command\nnext-command\n[exit 0]');
});

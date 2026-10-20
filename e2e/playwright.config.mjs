import {defineConfig, devices} from '@playwright/test';

const port = Number(process.env.OW_E2E_PORT || 18443);
export default defineConfig({
  testDir: 'tests',
  // Tests share one dashboard but each signs in as its own tailnet user.
  workers: 2,
  retries: process.env.CI ? 1 : 0,
  timeout: 30_000,
  reporter: [['list'], ['json', {outputFile: 'playwright-report/results.json'}]],
  use: {
    ...devices['Desktop Chrome'],
    baseURL: 'https://dashboard.e2e.ts.net',
    ignoreHTTPSErrors: true,
    // The dashboard accepts only its configured Host and exact https Origin.
    launchOptions: {args: [`--host-resolver-rules=MAP dashboard.e2e.ts.net 127.0.0.1:${port}`]},
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [
    {name: 'dashboard', testIgnore: /hosts\.spec/},
    // Inviting a host changes placement for every user, so this runs alone, last.
    {name: 'host-pool', testMatch: /hosts\.spec/, dependencies: ['dashboard']},
  ],
  webServer: {
    command: 'node harness/start.mjs',
    port,
    reuseExistingServer: false,
    stdout: 'pipe',
    timeout: 60_000,
  },
});

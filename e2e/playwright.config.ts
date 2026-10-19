import { defineConfig } from "@playwright/test";

// Browser end-to-end tests for the `furmi` dashboard. Every test starts its own
// `furmi web`/`furmi start` against a throwaway home, git repo and tmux server (see
// tests/fixtures.ts), so tests are independent and can run in parallel.
//
// The binary under test is $FURMI_BIN, defaulting to the debug build:
//   cargo build && cd e2e && npm ci && npx playwright install chromium && npm test
export default defineConfig({
  testDir: "./tests",
  timeout: 60_000,
  expect: { timeout: 15_000 },
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0, // a retry would hide a flaky dashboard
  workers: process.env.CI ? 2 : undefined,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
});

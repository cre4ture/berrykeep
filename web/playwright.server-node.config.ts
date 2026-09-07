import { defineConfig, devices } from "@playwright/test";
import { playwrightWebGlLaunchOptions } from "./tests/playwright-webgl";

export default defineConfig({
  testDir: "./tests",
  timeout: 90_000,
  expect: {
    timeout: 10_000
  },
  use: {
    baseURL: "https://127.0.0.1:18181",
    trace: "retain-on-failure",
    ignoreHTTPSErrors: true,
    launchOptions: playwrightWebGlLaunchOptions
  },
  webServer: {
    command: "node tests/scripts/run-server-node-runtime.mjs",
    url: "https://127.0.0.1:18181/api/v1/auth/admin/session",
    reuseExistingServer: false,
    timeout: 90_000,
    ignoreHTTPSErrors: true
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] }
    }
  ]
});

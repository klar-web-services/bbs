import { defineConfig, devices } from '@playwright/test'

export default defineConfig({
  testDir: './tests',
  timeout: 20_000,
  use: {
    baseURL: 'http://127.0.0.1:5174',
    trace: 'retain-on-failure',
    launchOptions: process.env.BBS_CHROMIUM_PATH ? { executablePath: process.env.BBS_CHROMIUM_PATH } : undefined,
  },
  webServer: {
    command: 'npm run dev -- --host 127.0.0.1 --port 5174',
    url: 'http://127.0.0.1:5174',
    reuseExistingServer: !process.env.CI,
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
})

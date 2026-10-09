import { defineConfig, devices } from "@playwright/test";

const CLIENT = "http://localhost:4301";
const VIEWER = "http://localhost:4302";

// Browser E2E against production builds of both apps (vite preview).
export default defineConfig({
  testDir: "tests",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["github"], ["html", { open: "never" }]] : "list",
  use: { trace: "retain-on-failure", locale: "de-DE" },
  projects: [
    {
      name: "desktop",
      use: { ...devices["Desktop Chrome"], viewport: { width: 1280, height: 800 } },
    },
    { name: "mobile", use: { ...devices["Pixel 7"] } },
  ],
  webServer: [
    {
      command:
        "pnpm --filter @fernsicht/client-ui build && pnpm --filter @fernsicht/client-ui preview --port 4301 --strictPort",
      url: CLIENT,
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
    {
      command:
        "pnpm --filter @fernsicht/viewer build && pnpm --filter @fernsicht/viewer preview --port 4302 --strictPort",
      url: VIEWER,
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
  ],
});

export { CLIENT, VIEWER };

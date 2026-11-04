import { defineConfig, devices } from "@playwright/test";

const CLIENT = "http://localhost:4301";
const VIEWER = "http://localhost:4302";

// Browser E2E against production builds of both apps (vite preview, on Bun
// as the runner is). Each server runs in its app's folder.
/** Builds the UI before serving it, unless CI brought it built
 * (FERNSICHT_UI_BUILT). */
const build = process.env.FERNSICHT_UI_BUILT ? "" : "bun run build && ";
const preview = (port: number) => `${build}bun --bun vite preview --port ${port} --strictPort`;

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
    // The same phone turned sideways: little height for toolbar and picture.
    { name: "mobile-landscape", use: { ...devices["Pixel 7 landscape"] } },
  ],
  webServer: [
    {
      command: preview(4301),
      cwd: "../client-ui",
      url: CLIENT,
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
    {
      command: preview(4302),
      cwd: "../viewer",
      url: VIEWER,
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
  ],
});

export { CLIENT, VIEWER };

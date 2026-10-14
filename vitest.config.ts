import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Unit and component tests for all web packages (jsdom). Browser E2E
// tests live in web/e2e and run with Playwright.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    setupFiles: ["./vitest.setup.ts"],
    include: ["{web/ui,web/viewer,web/client-ui,web/rendezvous-worker}/src/**/*.test.{ts,tsx}"],
    coverage: {
      provider: "v8",
      include: ["{web/ui,web/viewer,web/client-ui,web/rendezvous-worker}/src/**/*.{ts,tsx}"],
      // The Worker's entry is the runtime glue around lib/room.ts (tested).
      exclude: [
        "**/*.test.{ts,tsx}",
        "**/main.tsx",
        "**/*.d.ts",
        "web/rendezvous-worker/src/index.ts",
      ],
      reporter: ["text", "lcov"],
      thresholds: { lines: 90, functions: 90, branches: 85, statements: 90 },
    },
  },
});

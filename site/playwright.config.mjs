import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  outputDir: "./test-results",
  workers: 1,
  reporter: [["line"], ["json", { outputFile: "./test-results/report.json" }]],
  use: { browserName: "chromium", screenshot: "only-on-failure" },
  projects: [
    { name: "desktop", use: { viewport: { width: 1280, height: 900 } } },
    { name: "mobile", use: { viewport: { width: 390, height: 844 }, isMobile: true, deviceScaleFactor: 1 } },
  ],
});

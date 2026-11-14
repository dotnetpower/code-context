import { once } from "node:events";
import { readFileSync } from "node:fs";
import { expect, test } from "@playwright/test";
import { createAgentServer } from "../scripts/serve-agents.mjs";
import { BASE_PATH } from "../scripts/site-contract.mjs";

const index = JSON.parse(readFileSync(new URL("../dist/docs-index.json", import.meta.url), "utf8"));
let server;
let origin;
test.beforeAll(async () => {
  server = createAgentServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  origin = `http://127.0.0.1:${server.address().port}`;
});
test.afterAll(async () => {
  server.closeAllConnections();
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
});
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    window.__cctxCLS = { max: 0, value: 0, start: 0, last: 0 };
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        if (entry.hadRecentInput) continue;
        const state = window.__cctxCLS;
        if (state.last > 0 && entry.startTime - state.last < 1000 && entry.startTime - state.start < 5000) state.value += entry.value;
        else { state.value = entry.value; state.start = entry.startTime; }
        state.last = entry.startTime;
        state.max = Math.max(state.max, state.value);
      }
    }).observe({ type: "layout-shift", buffered: true });
  });
});

for (const entry of index.pages) {
  test(`static answers and CLS < 0.10: ${new URL(entry.url).pathname}`, async ({ page }, testInfo) => {
    await page.goto(`${origin}${new URL(entry.url).pathname}`, { waitUntil: "networkidle" });
    await page.evaluate(() => document.fonts.ready);
    await expect(page.locator("h1")).toHaveCount(1);
    const capsule = page.locator(".cctx-answer-capsule, .answer-capsule").first();
    await expect(capsule).toBeVisible();
    const box = await capsule.boundingBox();
    expect(box.y, "answer capsule must start above the fold").toBeLessThan(page.viewportSize().height);
    await page.evaluate(async () => {
      for (let y = 0; y < document.documentElement.scrollHeight; y += innerHeight) {
        scrollTo(0, y);
        await new Promise((resolve) => setTimeout(resolve, 80));
      }
    });
    await page.waitForTimeout(1000);
    const cls = await page.evaluate(() => window.__cctxCLS.max);
    await testInfo.attach("cls", { body: JSON.stringify({ url: entry.url, cls, viewport: page.viewportSize() }), contentType: "application/json" });
    expect(cls, "lab CLS; field CWV must be monitored separately").toBeLessThan(0.10);
  });
}

test("payload calculator is accurate and reserves its result area", async ({ page }) => {
  await page.goto(`${origin}${BASE_PATH}/`, { waitUntil: "networkidle" });
  const form = page.locator("#lookup-budget");
  await form.scrollIntoViewIfNeeded();
  const before = await form.locator("output").boundingBox();
  await form.getByLabel("Baseline tokens").fill("1000");
  await form.getByLabel("Clamped tokens").fill("250");
  await form.getByRole("button").click();
  await expect(form.locator("output")).toContainText("75.0% payload reduction (750 tokens)");
  const after = await form.locator("output").boundingBox();
  expect(after.height).toBe(before.height);
  await form.getByLabel("Clamped tokens").fill("1250");
  await form.getByRole("button").click();
  await expect(form.locator("output")).toContainText("-25.0% payload reduction (-250 tokens)");
  await form.getByLabel("Clamped tokens").fill("0");
  await form.getByRole("button").click();
  await expect(form.locator("output")).toContainText("100.0% payload reduction");
  await form.getByLabel("Baseline tokens").fill("0");
  await form.getByRole("button").click();
  expect(await form.locator('input[name="baseline"]').evaluate((input) => input.validity.valid)).toBe(false);
});

test("answers remain available without JavaScript", async ({ browser }) => {
  const context = await browser.newContext({ javaScriptEnabled: false });
  try {
    const page = await context.newPage();
    for (const entry of index.pages) {
      await page.goto(`${origin}${new URL(entry.url).pathname}`);
      await expect(page.locator(".cctx-answer-capsule, .answer-capsule").first()).toBeVisible();
      await expect(page.locator("h1")).toHaveCount(1);
    }
  } finally {
    await context.close();
  }
});

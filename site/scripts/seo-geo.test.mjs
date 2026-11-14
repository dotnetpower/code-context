import assert from "node:assert/strict";
import { once } from "node:events";
import { createServer } from "node:http";
import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { validateArtifacts, validateLive, validatePage } from "./check-seo-geo.mjs";
import { createAgentServer } from "./serve-agents.mjs";
import { BASE_PATH, INDEXNOW_KEY, prefersMarkdown, SITE_ROOT } from "./site-contract.mjs";
import { indexNowBatches, submitIndexing } from "./submit-indexing.mjs";

const dist = path.resolve(fileURLToPath(new URL("../dist/", import.meta.url)));

test("Accept negotiation respects quality, exclusions, wildcards, and tie order", () => {
  for (const accept of ["text/markdown", "text/markdown, text/html", "text/html;q=0.1, text/markdown;q=0.9", "TEXT/MARKDOWN; charset=utf-8", "text/markdown, */*;q=0.8"]) {
    assert.equal(prefersMarkdown(accept), true, accept);
  }
  for (const accept of ["", "*/*", "text/html", "text/html, text/markdown", "text/markdown;q=0", "text/markdown;q=0, */*;q=1", "text/html;q=1, text/markdown;q=0.5", "text/markdown;q=bogus", "text/markdown;q=2"]) {
    assert.equal(prefersMarkdown(accept), false, accept);
  }
});

test("page gate rejects social mismatches, long summaries, duplicate H1, empty alt, and malformed JSON-LD", () => {
  const html = readFileSync(path.join(dist, "index.html"), "utf8");
  const mutations = [
    [html.replace(/(<meta name="twitter:title" content=")[^"]*/, "$1Wrong title"), "title:social_mismatch"],
    [html.replace(/(<meta name="description" content=")[^"]*/, `$1${"x".repeat(161)}`), "description:over_160"],
    [html.replace("</main>", "<h1>Another title</h1></main>"), "h1:expected_one"],
    [html.replace("</main>", '<img alt="" src="/image.png" width="1" height="1"></main>'), "image:empty_alt:"],
    [html.replace(/(<script type="application\/ld\+json">)[\s\S]*?(<\/script>)/, "$1{$2"), "jsonld:invalid:"],
  ];
  for (const [source, expected] of mutations) {
    assert.ok(validatePage(source, `${SITE_ROOT}/`).some((failure) => failure.startsWith(expected)), expected);
  }
});

test("artifact gate detects sitemap drift, crawler denial and a fabricated API", async () => {
  const fixture = mkdtempSync(path.join(tmpdir(), "cctx-seo-contract-"));
  try {
    cpSync(dist, fixture, { recursive: true });
    const sitemap = path.join(fixture, "sitemap.xml");
    writeFileSync(sitemap, readFileSync(sitemap, "utf8").replace(/<url>[\s\S]*?<\/url>/, ""));
    const robots = path.join(fixture, "robots.txt");
    writeFileSync(robots, readFileSync(robots, "utf8").replace("User-agent: Googlebot\nAllow: /", "User-agent: Googlebot\nDisallow: /"));
    const openapi = path.join(fixture, "openapi.json");
    const spec = JSON.parse(readFileSync(openapi, "utf8"));
    spec.paths["/checkout"] = { post: {} };
    writeFileSync(openapi, JSON.stringify(spec));
    const { failures } = await validateArtifacts(fixture);
    for (const expected of ["sitemap:canonical_mismatch:sitemap.xml", "robots:wrong_policy:Googlebot", "openapi:only_real_read_endpoint"]) {
      assert.ok(failures.includes(expected), expected);
    }
  } finally {
    rmSync(fixture, { recursive: true, force: true });
  }
});

test("built artifacts and HTTP gateway pass strict discovery and alternating representation checks", async () => {
  const result = await validateArtifacts(dist);
  assert.deepEqual(result.failures, []);
  const server = createAgentServer({ dist });
  server.listen(0, "127.0.0.1");
  try {
    await once(server, "listening");
    const origin = `http://127.0.0.1:${server.address().port}`;
    assert.deepEqual(await validateLive(`${origin}${BASE_PATH}/`, { ...result, dist }), []);
    const redirect = await fetch(`${origin}${BASE_PATH}/tools`, { redirect: "manual" });
    assert.equal(redirect.status, 301);
    assert.equal(redirect.headers.get("location"), `${BASE_PATH}/tools/`);
    const htmlAlias = await fetch(`${origin}${BASE_PATH}/tools/index.html`, { redirect: "manual" });
    assert.equal(htmlAlias.status, 301);
    assert.equal(htmlAlias.headers.get("location"), `${BASE_PATH}/tools/`);
    const forbidden = await fetch(`${origin}${BASE_PATH}/docs-index.json`, { method: "POST" });
    assert.equal(forbidden.status, 405);
    const notFound = await fetch(`${origin}${BASE_PATH}/404.html`);
    assert.equal(notFound.status, 404);
    assert.equal(notFound.headers.get("x-robots-tag"), "noindex");
  } finally {
    server.closeAllConnections();
    await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
});

test("live gate rejects HTML-only hosts and missing cache isolation", async () => {
  const result = await validateArtifacts(dist);
  const homepage = result.pages.find((page) => page.relative === "index.html");
  const server = createServer((_req, res) => {
    res.writeHead(200, { "Content-Type": "text/html" });
    res.end(homepage.html);
  });
  server.listen(0, "127.0.0.1");
  try {
    await once(server, "listening");
    const failures = await validateLive(`http://127.0.0.1:${server.address().port}${BASE_PATH}/`, { ...result, pages: [homepage], dist });
    for (const prefix of ["http:cache_isolation:", "http:markdown_contamination:", "http:root_discovery:", "http:missing_link_header:", "http:not_honest_404:"]) {
      assert.ok(failures.some((failure) => failure.startsWith(prefix)), prefix);
    }
  } finally {
    server.closeAllConnections();
    await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
});

test("indexing only submits canonical batches and explicitly checks provider failures", async () => {
  const urls = Array.from({ length: 10001 }, (_, i) => `${SITE_ROOT}/page-${i}/`);
  assert.deepEqual(indexNowBatches(urls).map((batch) => batch.urlList.length), [10000, 1]);
  assert.throws(() => indexNowBatches(["https://example.invalid/"]), /outside/);
  assert.throws(() => indexNowBatches([`${SITE_ROOT}/`, `${SITE_ROOT}/`]), /unique/);
  const calls = [];
  const fetchImpl = async (url, options) => {
    calls.push({ url, options });
    return new Response(url.endsWith(".txt") ? INDEXNOW_KEY : "", { status: 200 });
  };
  assert.equal((await submitIndexing({ urls: [`${SITE_ROOT}/`], fetchImpl, accessToken: "test-only" })).submitted, true);
  assert.equal(calls[1].url, "https://api.indexnow.org/IndexNow");
  assert.equal(calls[1].options.method, "POST");
  assert.equal(calls[2].options.method, "PUT");
  await assert.rejects(submitIndexing({ urls: [`${SITE_ROOT}/`], fetchImpl, accessToken: "" }), /GSC_ACCESS_TOKEN/);
  await assert.rejects(submitIndexing({ urls: [`${SITE_ROOT}/`], accessToken: "test-only", fetchImpl: async () => new Response("denied", { status: 403 }) }), /verification failed/);
  for (const provider of ["indexnow.org", "googleapis.com"]) {
    await assert.rejects(submitIndexing({
      urls: [`${SITE_ROOT}/`], accessToken: "test-only",
      fetchImpl: async (url) => new Response(url.endsWith(".txt") ? INDEXNOW_KEY : "", { status: url.includes(provider) ? 403 : 200 }),
    }), /HTTP 403/);
  }
  const dryRun = await submitIndexing({ urls: [`${SITE_ROOT}/`], dryRun: true, fetchImpl: () => { throw new Error("Dry run must not contact a provider"); } });
  assert.equal(dryRun.submitted, false);
});

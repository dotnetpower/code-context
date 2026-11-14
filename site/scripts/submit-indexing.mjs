import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { INDEXNOW_KEY, SITE_ORIGIN, SITE_ROOT } from "./site-contract.mjs";

export function indexNowBatches(urls) {
  if (!urls.length || new Set(urls).size !== urls.length) throw new Error("IndexNow requires nonempty, unique canonical URLs");
  if (urls.some((url) => new URL(url).origin !== SITE_ORIGIN || !url.startsWith(`${SITE_ROOT}/`) || !url.endsWith("/"))) {
    throw new Error("IndexNow URL is outside the configured canonical site");
  }
  const batches = [];
  for (let i = 0; i < urls.length; i += 10000) {
    batches.push({ host: new URL(SITE_ROOT).host, key: INDEXNOW_KEY, keyLocation: `${SITE_ROOT}/${INDEXNOW_KEY}.txt`, urlList: urls.slice(i, i + 10000) });
  }
  return batches;
}

export async function submitIndexing({ urls, fetchImpl = fetch, accessToken = process.env.GSC_ACCESS_TOKEN, dryRun = false } = {}) {
  const batches = indexNowBatches(urls);
  const property = process.env.GSC_PROPERTY || `${SITE_ROOT}/`;
  const gsc = `https://www.googleapis.com/webmasters/v3/sites/${encodeURIComponent(property)}/sitemaps/${encodeURIComponent(`${SITE_ROOT}/sitemap.xml`)}`;
  if (dryRun) return { batches, gsc, submitted: false };
  if (!accessToken) throw new Error("GSC_ACCESS_TOKEN is required; grant sitemap submission access to the verified Search Console property");
  const verified = await fetchImpl(`${SITE_ROOT}/${INDEXNOW_KEY}.txt`, { redirect: "error", signal: AbortSignal.timeout(20000) });
  if (verified.status !== 200 || (await verified.text()).trim() !== INDEXNOW_KEY) throw new Error("Deployed IndexNow key verification failed");
  for (const payload of batches) {
    const response = await fetchImpl("https://api.indexnow.org/IndexNow", {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload), signal: AbortSignal.timeout(20000),
    });
    if (![200, 202].includes(response.status)) throw new Error(`IndexNow rejected the batch: HTTP ${response.status}`);
  }
  const response = await fetchImpl(gsc, { method: "PUT", headers: { Authorization: `Bearer ${accessToken}` }, signal: AbortSignal.timeout(20000) });
  if (!response.ok) throw new Error(`GSC sitemap submission failed: HTTP ${response.status}`);
  return { batches: batches.length, submitted: true };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const index = JSON.parse(readFileSync(new URL("../dist/docs-index.json", import.meta.url), "utf8"));
  submitIndexing({ urls: index.pages.map((p) => p.url), dryRun: process.argv.includes("--dry-run") })
    .then((result) => console.log(`[submit-indexing] ${JSON.stringify(result)}; acceptance is not an indexing guarantee`))
    .catch((error) => { console.error(`[submit-indexing] ${error.message}`); process.exitCode = 1; });
}

// build-seo-geo.mjs — Post-build SEO, GEO & AEO artifact generator for Code Context (cctx).
//
// 1. Syncs root `llms.txt` to `site/public/llms.txt` and `site/dist/llms.txt`.
// 2. Generates `site/dist/llms-full.txt` concatenating every published English page in
//    deterministic order with canonical URLs for one-shot generative-engine ingestion.
// 3. Generates clean `.md` mirrors (`dist/<slug>.md`, `dist/index.md`, and `dist/sitemap.md`)
//    so AI agents and answer engines can fetch zero-boilerplate Markdown directly.
// 4. Emits standards-compliant `sitemap-0.xml`, `sitemap.xml`, `sitemap-index.xml`, `rss.xml`,
//    and IndexNow verification key file.

import { readFileSync, writeFileSync, readdirSync, statSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const siteDir = path.resolve(scriptDir, "..");
const repoRoot = path.resolve(siteDir, "..");
const docsRoot = path.join(siteDir, "src", "content", "docs");
const publicDir = path.join(siteDir, "public");
const distDir = path.join(siteDir, "dist");

const SITE_URL = (process.env.SITE_URL ?? "https://dotnetpower.github.io").replace(/\/$/, "");
const BASE_PATH = (process.env.BASE_PATH ?? "/code-context").replace(/\/$/, "");
const BASE_URL = `${SITE_URL}${BASE_PATH}`;

const BUILD_ISO = "2026-11-14T03:00:00Z";
const BUILD_PUBDATE = new Date(BUILD_ISO).toUTCString();
const INDEXNOW_KEY = "c7d4e9a1b2f84c6093e5a7d21f0b8e4c";

function walkDocs(dir) {
  return readdirSync(dir)
    .sort()
    .flatMap((name) => {
      const full = path.join(dir, name);
      return statSync(full).isDirectory() ? walkDocs(full) : [full];
    })
    .filter((f) => /\.(md|mdx)$/.test(f));
}

function parseFrontmatterAndBody(raw) {
  const match = raw.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/);
  if (!match) return { meta: {}, body: raw.trim() };
  const [, fmText, body] = match;
  const meta = {};
  for (const line of fmText.split(/\r?\n/)) {
    const m = line.match(/^([a-zA-Z0-9_-]+):\s*(.+)$/);
    if (m) {
      meta[m[1]] = m[2].replace(/^["']|["']$/g, "").trim();
    }
  }
  return { meta, body: body.trim() };
}

function cleanMdxBody(body) {
  return body
    .replace(/^import\s+[^\n]+\n?/gm, "")
    .replace(/^export\s+const\s+[^\n]+\n?/gm, "")
    .replace(/<MotionLanding[^>]*\/>/g, "")
    .replace(/<figure[\s\S]*?<\/figure>/g, "")
    .trim();
}

function docCanonicalUrl(relPath) {
  let slug = relPath.replace(/\.(md|mdx)$/, "");
  if (slug === "index") return `${BASE_URL}/`;
  if (slug.endsWith("/index")) slug = slug.slice(0, -"/index".length);
  return `${BASE_URL}/${slug}/`;
}

function docSlug(relPath) {
  return relPath.replace(/\.(md|mdx)$/, "");
}

function xmlEscape(value) {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

// 1. Sync root llms.txt & IndexNow key
const rootLlmsPath = path.join(repoRoot, "llms.txt");
const rootLlmsText = existsSync(rootLlmsPath) ? readFileSync(rootLlmsPath, "utf8") : "";
if (rootLlmsText) {
  writeFileSync(path.join(publicDir, "llms.txt"), rootLlmsText, "utf8");
  if (existsSync(distDir)) {
    writeFileSync(path.join(distDir, "llms.txt"), rootLlmsText, "utf8");
  }
}

writeFileSync(path.join(publicDir, `${INDEXNOW_KEY}.txt`), `${INDEXNOW_KEY}\n`, "utf8");
if (existsSync(distDir)) {
  writeFileSync(path.join(distDir, `${INDEXNOW_KEY}.txt`), `${INDEXNOW_KEY}\n`, "utf8");
}

// 2. Order documentation pages deterministically (index first, then sidebar order)
const ORDER_PRIORITY = [
  "index",
  "installation",
  "quickstart",
  "tools",
  "probes-and-journal",
  "benchmarks",
  "architecture",
  "faq",
];

const allDocs = walkDocs(docsRoot)
  .map((f) => {
    const relPath = path.relative(docsRoot, f).split(path.sep).join("/");
    return { fullPath: f, relPath, slug: docSlug(relPath) };
  })
  .sort((a, b) => {
    const ia = ORDER_PRIORITY.indexOf(a.slug);
    const ib = ORDER_PRIORITY.indexOf(b.slug);
    if (ia !== -1 && ib !== -1) return ia - ib;
    if (ia !== -1) return -1;
    if (ib !== -1) return 1;
    return a.slug.localeCompare(b.slug);
  });

const fullSections = [
  `# Code Context (cctx) — Complete English Documentation Corpus

Website: ${BASE_URL}/
Repository: https://github.com/dotnetpower/code-context
VS Code Marketplace: https://marketplace.visualstudio.com/items?itemName=dotnetpower.code-context-native
Structured Index: ${BASE_URL}/llms.txt
Markdown Sitemap: ${BASE_URL}/sitemap.md
Generated for search and generative-engine (GEO / AEO) ingestion.
`,
];

const sitemapMdLines = [
  `# Code Context (cctx) — Semantic Documentation Map`,
  ``,
  `- Website: ${BASE_URL}/`,
  `- Structured LLM Index: ${BASE_URL}/llms.txt`,
  `- Full Single-Fetch Corpus: ${BASE_URL}/llms-full.txt`,
  ``,
  `## Pages`,
  ``,
];

const rssItems = [];
const sitemapEntries = [];

for (const { fullPath, relPath, slug } of allDocs) {
  const raw = readFileSync(fullPath, "utf8");
  const { meta, body } = parseFrontmatterAndBody(raw);
  const cleaned = cleanMdxBody(body);
  const url = docCanonicalUrl(relPath);
  const mdUrl = `${BASE_URL}/${slug}.md`;
  const title = meta.title || slug;
  const desc =
    meta.description ||
    "Fast local file discovery, trigram-indexed source search, and block-clamped context cache for AI coding agents.";

  const effectiveBody =
    slug === "index" && !cleaned
      ? rootLlmsText
      : cleaned;

  // Per-page .md mirror in dist/
  if (existsSync(distDir)) {
    const outMdPath = path.join(distDir, `${slug}.md`);
    mkdirSync(path.dirname(outMdPath), { recursive: true });
    const mirrorContent = `---\ntitle: "${title}"\ndescription: "${desc}"\nurl: "${url}"\n---\n\n# ${title}\n\n> ${desc}\n\n${effectiveBody}\n`;
    writeFileSync(outMdPath, mirrorContent, "utf8");
  }

  sitemapMdLines.push(`- [${title}](${url}) — ${desc} ([Markdown](${mdUrl}))`);

  rssItems.push(
    `    <item>\n` +
      `      <title>${xmlEscape(title)}</title>\n` +
      `      <link>${xmlEscape(url)}</link>\n` +
      `      <guid isPermaLink="true">${xmlEscape(url)}</guid>\n` +
      `      <description>${xmlEscape(desc)}</description>\n` +
      `      <pubDate>${BUILD_PUBDATE}</pubDate>\n` +
      `    </item>`,
  );

  sitemapEntries.push(url);

  fullSections.push(
    `\n---\n\n## ${title}\n\nCanonical URL: <${url}>\nMarkdown Mirror: <${mdUrl}>\n\n> ${desc}\n\n${effectiveBody}\n`,
  );
}

if (existsSync(distDir)) {
  writeFileSync(path.join(distDir, "llms-full.txt"), fullSections.join(""), "utf8");
  writeFileSync(path.join(distDir, "sitemap.md"), `${sitemapMdLines.join("\n")}\n`, "utf8");

  // 3. Emit sitemap-0.xml, sitemap.xml, and sitemap-index.xml
  const urlNodes = sitemapEntries.map(
    (loc) =>
      `  <url>\n` +
      `    <loc>${xmlEscape(loc)}</loc>\n` +
      `    <lastmod>${BUILD_ISO}</lastmod>\n` +
      `    <xhtml:link rel="alternate" hreflang="en" href="${xmlEscape(loc)}"/>\n` +
      `    <xhtml:link rel="alternate" hreflang="x-default" href="${xmlEscape(loc)}"/>\n` +
      `  </url>`,
  );

  const sitemapXml =
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:xhtml="http://www.w3.org/1999/xhtml">\n` +
    `${urlNodes.join("\n")}\n` +
    `</urlset>\n`;

  writeFileSync(path.join(distDir, "sitemap-0.xml"), sitemapXml, "utf8");
  writeFileSync(path.join(distDir, "sitemap.xml"), sitemapXml, "utf8");

  const sitemapIndexXml =
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n` +
    `  <sitemap>\n` +
    `    <loc>${xmlEscape(`${BASE_URL}/sitemap-0.xml`)}</loc>\n` +
    `    <lastmod>${BUILD_ISO}</lastmod>\n` +
    `  </sitemap>\n` +
    `</sitemapindex>\n`;
  writeFileSync(path.join(distDir, "sitemap-index.xml"), sitemapIndexXml, "utf8");

  // 4. Emit RSS 2.0 feed
  const rssXml =
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">\n` +
    `  <channel>\n` +
    `    <title>Code Context (cctx) Documentation</title>\n` +
    `    <link>${xmlEscape(`${BASE_URL}/`)}</link>\n` +
    `    <description>Fast local file discovery, trigram-indexed source search, and block-clamped context cache for AI coding agents.</description>\n` +
    `    <language>en-us</language>\n` +
    `    <lastBuildDate>${BUILD_PUBDATE}</lastBuildDate>\n` +
    `    <atom:link href="${xmlEscape(`${BASE_URL}/rss.xml`)}" rel="self" type="application/rss+xml"/>\n` +
    `${rssItems.join("\n")}\n` +
    `  </channel>\n` +
    `</rss>\n`;
  writeFileSync(path.join(distDir, "rss.xml"), rssXml, "utf8");

  console.log(`[build-seo-geo] generated SEO/GEO/AEO artifacts for ${allDocs.length} pages at ${BASE_URL}`);
}

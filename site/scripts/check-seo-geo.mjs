// check-seo-geo.mjs — Automated SEO, GEO & AEO verification gate for Code Context (cctx).
// Run after `npm --prefix site run build` via `npm --prefix site run check:seo`.

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const siteDir = path.resolve(scriptDir, "..");
const repoRoot = path.resolve(siteDir, "..");
const distDir = path.join(siteDir, "dist");

const SITE_ORIGIN = (process.env.SITE_URL ?? "https://dotnetpower.github.io").replace(/\/$/, "");
const BASE_PATH = (process.env.BASE_PATH ?? "/code-context").replace(/\/$/, "");
const SITE_ROOT = `${SITE_ORIGIN}${BASE_PATH}`;

const REQUIRED_BOTS = [
  "Googlebot",
  "bingbot",
  "Yeti",
  "DuckDuckBot",
  "OAI-SearchBot",
  "ChatGPT-User",
  "Claude-SearchBot",
  "Claude-User",
  "PerplexityBot",
  "GPTBot",
  "ClaudeBot",
  "Google-Extended",
  "Applebot-Extended",
  "CCBot",
];

const EXPECTED_SLUGS = [
  "index",
  "installation",
  "quickstart",
  "tools",
  "probes-and-journal",
  "benchmarks",
  "architecture",
  "faq",
];

function walkHtml(dir) {
  return readdirSync(dir)
    .sort()
    .flatMap((name) => {
      const full = path.join(dir, name);
      return statSync(full).isDirectory() ? walkHtml(full) : [full];
    })
    .filter((f) => f.endsWith(".html"));
}

const failures = [];

if (!existsSync(distDir)) {
  console.error(`[check-seo-geo] dist directory missing: ${distDir}`);
  process.exit(1);
}

// 1. OG cover PNG signature check
const ogPath = path.join(distDir, "og-cover.png");
if (!existsSync(ogPath)) {
  failures.push("missing_asset:og-cover.png");
} else {
  const sig = readFileSync(ogPath).subarray(0, 8).toString("hex");
  if (sig !== "89504e470d0a1a0a") {
    failures.push(`invalid_png_signature:og-cover.png:${sig}`);
  }
}

// 2. Benchmark SVG assets check
for (const fig of ["latency-speedup.svg", "cold-start.svg"]) {
  if (!existsSync(path.join(distDir, "benchmarks", fig))) {
    failures.push(`missing_benchmark_asset:${fig}`);
  }
}

// 3. robots.txt crawler & sitemap checks
const robotsPath = path.join(distDir, "robots.txt");
if (!existsSync(robotsPath)) {
  failures.push("missing_asset:robots.txt");
} else {
  const robotsText = readFileSync(robotsPath, "utf8");
  for (const bot of REQUIRED_BOTS) {
    if (!robotsText.includes(`User-agent: ${bot}`)) {
      failures.push(`robots_missing_bot:${bot}`);
    }
  }
  if (!robotsText.includes(`Sitemap: ${SITE_ROOT}/sitemap-index.xml`)) {
    failures.push("robots_missing_sitemap_index");
  }
  if (!robotsText.includes(`Sitemap: ${SITE_ROOT}/sitemap.xml`)) {
    failures.push("robots_missing_sitemap_xml");
  }
}

// 4. Sitemaps, RSS, llms.txt, llms-full.txt & .md mirrors
for (const asset of [
  "sitemap-index.xml",
  "sitemap-0.xml",
  "sitemap.xml",
  "rss.xml",
  "llms.txt",
  "llms-full.txt",
  "sitemap.md",
  "c7d4e9a1b2f84c6093e5a7d21f0b8e4c.txt",
]) {
  if (!existsSync(path.join(distDir, asset))) {
    failures.push(`missing_asset:${asset}`);
  }
}

if (existsSync(path.join(distDir, "llms.txt")) && existsSync(path.join(repoRoot, "llms.txt"))) {
  if (readFileSync(path.join(distDir, "llms.txt"), "utf8") !== readFileSync(path.join(repoRoot, "llms.txt"), "utf8")) {
    failures.push("llms_txt_out_of_sync_with_root");
  }
}

for (const slug of EXPECTED_SLUGS) {
  if (!existsSync(path.join(distDir, `${slug}.md`))) {
    failures.push(`missing_markdown_mirror:${slug}.md`);
  }
}

// 5. Inspect every built HTML page for SEO/GEO/AEO metadata, JSON-LD, and privacy hygiene
const htmlFiles = walkHtml(distDir);
for (const htmlFile of htmlFiles) {
  const rel = path.relative(distDir, htmlFile).split(path.sep).join("/");
  const html = readFileSync(htmlFile, "utf8");
  const is404 = rel === "404.html" || rel.endsWith("/404/index.html");

  if (/(?:\/home\/|\/Users\/|[A-Z]:\\Users\\)[^<\s"'/\\]+/.test(html)) {
    failures.push(`machine_path_leak:${rel}`);
  }

  if (!/<title>[^<]{10,}<\/title>/.test(html)) {
    failures.push(`missing_or_short_title:${rel}`);
  }
  if (!/<meta\s+name="description"\s+content="[^"]{40,}"/i.test(html)) {
    failures.push(`missing_or_short_description:${rel}`);
  }
  if (!/<link\s+rel="canonical"\s+href="https:\/\/[^"]+"/i.test(html)) {
    failures.push(`missing_canonical:${rel}`);
  }
  if (!/<meta\s+property="og:image"\s+content="https:\/\/[^"]+og-cover\.png"/i.test(html)) {
    failures.push(`missing_og_image:${rel}`);
  }
  if (!/<meta\s+name="twitter:card"\s+content="summary_large_image"/i.test(html)) {
    failures.push(`missing_twitter_card:${rel}`);
  }

  if (!is404) {
    const ldMatch = html.match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/i);
    if (!ldMatch) {
      failures.push(`missing_json_ld:${rel}`);
    } else {
      try {
        const parsed = JSON.parse(ldMatch[1]);
        if (!Array.isArray(parsed["@graph"]) || parsed["@graph"].length < 3) {
          failures.push(`invalid_json_ld_graph:${rel}`);
        }
      } catch (err) {
        failures.push(`malformed_json_ld:${rel}:${String(err)}`);
      }
    }
  }
}

if (failures.length > 0) {
  console.error(`[check-seo-geo] FAILED (${failures.length} issue(s)):`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}

console.log(`[check-seo-geo] OK — verified ${htmlFiles.length} HTML pages, OG cover, sitemaps, RSS, llms.txt/llms-full.txt, and ${EXPECTED_SLUGS.length} Markdown mirrors.`);

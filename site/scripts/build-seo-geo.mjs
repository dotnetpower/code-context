import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { attr, BASE_PATH, elements, INDEXNOW_KEY, LIMITATIONS, loadPages, markdownForPage, SITE_ROOT, text, xmlEscape as xml } from "./site-contract.mjs";

const dist = fileURLToPath(new URL("../dist/", import.meta.url));
const write = (name, value) => {
  const target = path.join(dist, name);
  mkdirSync(path.dirname(target), { recursive: true });
  writeFileSync(target, value);
};
const json = (name, value) => write(name, `${JSON.stringify(value, null, 2)}\n`);
const pages = loadPages(dist).filter((page) => page.indexable);
if (!pages.length) throw new Error("No indexable rendered pages");
const corpus = [`# Code Context (cctx) documentation\n\n${LIMITATIONS}\n`];
const manifest = [];
for (const page of pages) {
  const pathname = new URL(page.canonical).pathname;
  const slug = pathname.slice(BASE_PATH.length).replace(/^\/|\/$/g, "");
  const markdown = slug ? `${slug}.md` : "index.md";
  const content = markdownForPage(page);
  write(markdown, content);
  manifest.push({ title: page.title, description: page.description, url: page.canonical, markdown: `${SITE_ROOT}/${markdown}` });
  corpus.push(`\n---\n\n${content}`);

  // Schema answers are extracted from visible HTML, never independently invented.
  const main = elements(page.doc, (n) => n.tagName === "main")[0];
  const questions = elements(main, (n) => n.tagName === "details").flatMap((node) => {
    const summary = node.childNodes.find((n) => n.tagName === "summary");
    const answer = node.childNodes.filter((n) => n !== summary).map(text).join(" ").replace(/\s+/g, " ").trim();
    return summary && answer ? [{ "@type": "Question", name: text(summary).trim(), acceptedAnswer: { "@type": "Answer", text: answer } }] : [];
  });
  const schema = elements(page.doc, (n) => n.tagName === "script" && attr(n, "type") === "application/ld+json");
  if (schema.length !== 1) throw new Error(`Expected one graph: ${page.relative}`);
  const graph = JSON.parse(text(schema[0]));
  graph["@graph"] = graph["@graph"].filter((node) => node["@type"] !== "FAQPage");
  if (questions.length) graph["@graph"].push({ "@type": "FAQPage", "@id": `${page.canonical}#faq`, url: page.canonical, mainEntity: questions });
  writeFileSync(page.file, page.html.replace(text(schema[0]), JSON.stringify(graph).replace(/</g, "\\u003c")));
}

json("docs-index.json", { name: "Code Context documentation index", language: "en", limitations: LIMITATIONS, pages: manifest });
write("llms-full.txt", corpus.join("\n"));
write("llms.txt", `# Code Context (cctx)\n\n> Local Rust source indexing and block-clamped context for AI coding agents. This is an English documentation site, not a hosted workspace API.\n\n## Canonical pages\n\n${manifest.map((p) => `- [${p.title}](${p.url}): ${p.description}`).join("\n")}\n\n## Agent resources\n\n- [Full corpus](${SITE_ROOT}/llms-full.txt)\n- [Documentation JSON index](${SITE_ROOT}/docs-index.json)\n- [OpenAPI](${SITE_ROOT}/openapi.json)\n- [Authentication policy](${SITE_ROOT}/auth.md)\n\n## Sources and limitations\n\n${LIMITATIONS}\n\nSources: [published raw benchmark results](https://github.com/dotnetpower/code-context/tree/main/benchmarks/results/aks-store-demo) and the canonical documentation above. No ratings, commerce, OAuth provisioning or remote code-search service are offered.\n`);
write("sitemap.md", `# Documentation map\n\n${manifest.map((p) => `- [${p.title}](${p.url}) ([Markdown](${p.markdown}))`).join("\n")}\n`);
write("robots.txt", readFileSync(new URL("../public/robots.txt", import.meta.url), "utf8").replaceAll("https://dotnetpower.github.io/code-context", SITE_ROOT));
write(`${INDEXNOW_KEY}.txt`, `${INDEXNOW_KEY}\n`);

const urlset = (urls) => `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls.map((url) => `  <url><loc>${xml(url)}</loc></url>`).join("\n")}\n</urlset>\n`;
const urls = manifest.map((p) => p.url);
const shards = [];
for (let i = 0; i < urls.length; i += 4000) {
  const name = `sitemap-${i / 4000}.xml`;
  shards.push(name);
  write(name, urlset(urls.slice(i, i + 4000)));
}
const sitemapIndex = `<?xml version="1.0" encoding="UTF-8"?>\n<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${shards.map((name) => `  <sitemap><loc>${xml(`${SITE_ROOT}/${name}`)}</loc></sitemap>`).join("\n")}\n</sitemapindex>\n`;
write("sitemap-index.xml", sitemapIndex);
write("sitemap.xml", urls.length <= 4000 ? urlset(urls) : sitemapIndex);
write("rss.xml", `<?xml version="1.0" encoding="UTF-8"?>\n<rss version="2.0"><channel><title>Code Context documentation</title><link>${xml(SITE_ROOT)}/</link><description>Local code-context tooling documentation</description>${manifest.map((p) => `<item><title>${xml(p.title)}</title><link>${xml(p.url)}</link><guid isPermaLink="true">${xml(p.url)}</guid><description>${xml(p.description)}</description></item>`).join("\n")}</channel></rss>\n`);

write("auth.md", `# auth.md\n\n## Service\n\nCode Context documentation: ${SITE_ROOT}/\n\n## Authentication\n\nPublic read-only access is **anonymous**. Use unauthenticated GET requests to ${SITE_ROOT}/docs-index.json and the published documentation resources. No Authorization header, API key, OAuth flow, bot account or automatic account issuer is used. No write, purchase, payment or remote workspace operations exist here. Local daemon authentication is separate and is not exposed by this website.\n\n## Discovery and representations\n\nOpenAPI: ${SITE_ROOT}/openapi.json\nAPI catalog: ${SITE_ROOT}/.well-known/api-catalog\nFull corpus: ${SITE_ROOT}/llms-full.txt\n\nExplicit .md mirrors work on static hosting. Accept: text/markdown negotiation requires the provided HTTP gateway; GitHub Pages alone cannot implement it. Check HTTP response headers rather than assuming deployment support.\n\n## Limitations\n\n${LIMITATIONS}\n`);
json(".well-known/api-catalog", {
  linkset: [{
    anchor: `${SITE_ROOT}/.well-known/api-catalog`,
    item: [{ href: `${SITE_ROOT}/docs-index.json`, type: "application/json" }],
  }, {
    anchor: `${SITE_ROOT}/docs-index.json`,
    "service-desc": [{ href: `${SITE_ROOT}/openapi.json`, type: "application/openapi+json" }],
    "service-doc": [{ href: `${SITE_ROOT}/auth.md`, type: "text/markdown" }],
  }],
});
json("openapi.json", {
  openapi: "3.1.0",
  info: { title: "Code Context public documentation index", version: "1.0.0", description: "Static documentation metadata only. No local workspace access, writes, commerce or account provisioning." },
  servers: [{ url: SITE_ROOT }],
  security: [],
  paths: {
    "/docs-index.json": {
      get: {
        operationId: "getDocumentationIndex", summary: "List published English documentation pages",
        responses: {
          200: {
            description: "Canonical titles, summaries and Markdown URLs",
            content: { "application/json": { schema: {
              type: "object", required: ["name", "language", "limitations", "pages"],
              properties: {
                name: { type: "string" }, language: { const: "en" }, limitations: { type: "string" },
                pages: { type: "array", items: {
                  type: "object", required: ["title", "description", "url", "markdown"],
                  properties: Object.fromEntries(["title", "description", "url", "markdown"].map((key) => [key, { type: "string", ...(["url", "markdown"].includes(key) ? { format: "uri" } : {}) }])),
                } },
              },
            } } },
          },
        },
      },
    },
  },
});
console.log(`[build-seo-geo] ${pages.length} rendered canonical pages; corpus, discovery, sitemaps and RSS generated at ${SITE_ROOT}/`);

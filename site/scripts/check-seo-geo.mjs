import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";
import { XMLParser } from "fast-xml-parser";
import { attr, BASE_PATH, discoveryLink, elements, htmlDocument, INDEXNOW_KEY, links, loadPages, meta, SEARCH_BOTS, SITE_ORIGIN, SITE_ROOT, text, TRAINING_BOTS } from "./site-contract.mjs";

const defaultDist = path.resolve(fileURLToPath(new URL("../dist/", import.meta.url)));
const normalize = (value) => value.replace(/\s+/g, " ").trim();
const array = (value) => value === undefined ? [] : Array.isArray(value) ? value : [value];

export function validatePage(html, expectedCanonical) {
  const failures = [];
  const check = (condition, name) => { if (!condition) failures.push(name); };
  const doc = htmlDocument(html);
  const oneMeta = (key) => {
    const values = meta(doc, key);
    check(values.length === 1 && Boolean(values[0]?.trim()), `meta:${key}:expected_one`);
    return values[0];
  };
  const titles = elements(doc, (n) => n.tagName === "title");
  const title = text(titles[0]);
  check(titles.length === 1 && title.trim().length > 0, "title:expected_one");
  check(/Code Context|cctx|Architecture|Benchmarks|FAQ|Frequently Asked|Installation|Probes|Quickstart|Native VS Code/i.test(title.slice(0, 32)), "title:keyword_not_frontloaded");
  const description = oneMeta("description");
  check([...description ?? ""].length <= 160, "description:over_160");
  check(oneMeta("og:title") === title && oneMeta("twitter:title") === title, "title:social_mismatch");
  check(oneMeta("og:description") === description && oneMeta("twitter:description") === description, "description:social_mismatch");
  const headings = elements(doc, (n) => n.tagName === "h1");
  check(headings.length === 1 && Boolean(text(headings[0]).trim()), "h1:expected_one");
  check(elements(doc, (n) => n.tagName === "dl").length > 0, "answer:missing_static_summary");
  check(elements(doc, (n) => n.tagName === "details" && elements(n, (child) => child.tagName === "summary" && text(child).trim().endsWith("?")).length > 0).length > 0, "answer:missing_question");
  const canonicals = links(doc, "canonical");
  check(canonicals.length === 1 && attr(canonicals[0], "href") === expectedCanonical && expectedCanonical?.endsWith("/"), "canonical:not_self_referencing");
  check(oneMeta("og:url") === expectedCanonical, "og:url:mismatch");
  check(!links(doc, "alternate").some((node) => attr(node, "hreflang")), "hreflang:single_language_site");
  for (const resource of ["llms.txt", "llms-full.txt"]) {
    check(links(doc, "alternate").some((n) => attr(n, "href") === `${SITE_ROOT}/${resource}`), `alternate:${resource}`);
  }
  const ogImage = oneMeta("og:image");
  check(oneMeta("twitter:image") === ogImage, "social_image:mismatch");
  check(oneMeta("og:image:width") === "1200" && oneMeta("og:image:height") === "630", "social_image:dimensions");
  check(["image/png", "image/webp"].includes(oneMeta("og:image:type")), "social_image:type");
  check(oneMeta("twitter:card") === "summary_large_image", "twitter:card");
  for (const image of elements(doc, (n) => n.tagName === "img")) {
    check((attr(image, "alt")?.trim().length ?? 0) >= 3, `image:empty_alt:${attr(image, "src")}`);
    check(Boolean(attr(image, "width")) && Boolean(attr(image, "height")), `image:unreserved_dimensions:${attr(image, "src")}`);
  }
  const attributeErrors = [];
  const parsedWithLocations = htmlDocument(html, { sourceCodeLocationInfo: true, onParseError: (error) => {
    if (/attribute|unexpected-null/.test(error.code)) attributeErrors.push(error.code);
  } });
  check(attributeErrors.length === 0, "html:invalid_attribute_escaping");
  for (const image of elements(parsedWithLocations, (n) => n.tagName === "img")) {
    for (const location of Object.values(image.sourceCodeLocation?.attrs ?? {})) {
      const source = html.slice(location.startOffset, location.endOffset);
      check(!/[<>]/.test(source), "image:unescaped_attribute");
    }
  }
  const scripts = elements(doc, (n) => n.tagName === "script" && attr(n, "type") === "application/ld+json");
  check(scripts.length === 1, "jsonld:expected_one_graph");
  for (const script of scripts) {
    try {
      const parsed = JSON.parse(text(script));
      const graph = parsed["@graph"];
      check(parsed["@context"] === "https://schema.org" && Array.isArray(graph), "jsonld:graph");
      for (const type of ["Organization", "WebSite", "WebPage"]) {
        check(graph?.some((n) => n["@type"] === type), `jsonld:missing:${type}`);
      }
      const webpage = graph?.find((n) => n["@type"] === "WebPage");
      check(webpage?.url === expectedCanonical && webpage?.["@id"] === `${expectedCanonical}#webpage`, "jsonld:canonical");
      for (const node of graph ?? []) {
        if (["TechArticle", "FAQPage"].includes(node["@type"])) check(node.url === expectedCanonical && node["@id"].startsWith(expectedCanonical), "jsonld:page_url");
        check(!node.aggregateRating && !node.review && !node.offers, "jsonld:unsupported_claim");
        if (node["@type"] === "FAQPage") {
          const visible = normalize(text(elements(doc, (n) => n.tagName === "main")[0]));
          check(node.mainEntity.length > 0, "jsonld:empty_faq");
          for (const question of node.mainEntity) {
            check(visible.includes(normalize(question.name)) && visible.includes(normalize(question.acceptedAnswer.text)), "jsonld:faq_not_visible");
          }
        }
      }
    } catch (error) {
      failures.push(`jsonld:invalid:${error.message}`);
    }
  }
  check(!/(?:\/home\/|\/Users\/|[A-Z]:\\Users\\)[^<\s"'/\\]+/.test(html), "machine_path_leak");
  return failures;
}

export async function validateArtifacts(dist = defaultDist) {
  const failures = [];
  const check = (condition, name) => { if (!condition) failures.push(name); };
  const read = (name) => readFileSync(path.join(dist, name), "utf8");
  const json = (name) => JSON.parse(read(name));
  const xml = (name) => {
    const source = read(name);
    return new XMLParser().parse(source, true);
  };
  const allPages = loadPages(dist);
  const pages = allPages.filter((p) => p.indexable);
  check(pages.length > 0, "no_indexable_pages");
  const titles = new Set();
  const canonicals = new Set();
  const images = new Map();
  const resolveLocal = (href, base = `${SITE_ROOT}/`) => {
    const url = new URL(href, base);
    if (url.origin !== SITE_ORIGIN || (BASE_PATH && !url.pathname.startsWith(`${BASE_PATH}/`))) return null;
    const relative = decodeURIComponent(url.pathname.slice(BASE_PATH.length)).replace(/^\/+/, "");
    const file = path.resolve(dist, relative.endsWith("/") || !relative ? `${relative}index.html` : relative);
    check(file.startsWith(`${dist}${path.sep}`), `path:outside_dist:${href}`);
    return { file, url, relative };
  };
  for (const page of pages) {
    const expected = `${SITE_ROOT}/${page.relative.replace(/index\.html$/, "")}`;
    failures.push(...validatePage(page.html, expected).map((name) => `${page.relative}:${name}`));
    const title = normalize(page.title).toLowerCase();
    check(!titles.has(title), `duplicate_title:${page.title}`);
    titles.add(title);
    check(!canonicals.has(page.canonical), `duplicate_canonical:${page.canonical}`);
    canonicals.add(page.canonical);
    for (const node of elements(page.doc, (n) => ["a", "img", "link", "script"].includes(n.tagName))) {
      const href = attr(node, node.tagName === "img" || node.tagName === "script" ? "src" : "href");
      if (!href || /^(mailto:|tel:|data:|javascript:)/i.test(href)) continue;
      const local = resolveLocal(href, page.canonical);
      if (!local) continue;
      check(existsSync(local.file), `broken_internal_link:${page.relative}:${href}`);
      if (!existsSync(local.file)) {
        check(!existsSync(path.join(local.file, "index.html")), `redirecting_internal_link:${page.relative}:${href}`);
      } else if (node.tagName === "a" && local.url.hash && local.file.endsWith(".html")) {
        const target = allPages.find((p) => p.file === local.file);
        const id = decodeURIComponent(local.url.hash.slice(1));
        check(target && elements(target.doc, (n) => attr(n, "id") === id || attr(n, "name") === id).length > 0, `broken_fragment:${href}`);
      }
    }
    const imageUrl = meta(page.doc, "og:image")[0];
    if (!images.has(imageUrl)) {
      const local = resolveLocal(imageUrl);
      check(Boolean(local) && existsSync(local.file), `social_image:missing:${imageUrl}`);
      if (local && existsSync(local.file)) {
        const bytes = readFileSync(local.file);
        const info = await sharp(bytes).metadata();
        await sharp(bytes).raw().toBuffer();
        check(info.width === 1200 && info.height === 630 && ["png", "webp"].includes(info.format), "social_image:invalid_raster");
        check(info.format === "png" ? bytes.subarray(0, 8).toString("hex") === "89504e470d0a1a0a"
          : bytes.toString("ascii", 0, 4) === "RIFF" && bytes.toString("ascii", 8, 12) === "WEBP", "social_image:magic_bytes");
        images.set(imageUrl, info.format);
      }
    }
    check(meta(page.doc, "og:image:type")[0] === `image/${images.get(imageUrl)}`, `social_image:mime_mismatch:${page.relative}`);
  }
  const notFound = allPages.find((p) => p.relative === "404.html");
  check(notFound && !notFound.indexable, "404:missing_noindex");
  const sitemapUrls = (name) => {
    const document = xml(name);
    if (document.sitemapindex) return array(document.sitemapindex.sitemap).flatMap((entry) => sitemapUrls(new URL(entry.loc).pathname.slice(BASE_PATH.length + 1)));
    const entries = array(document.urlset?.url);
    check(entries.length <= 4000, `sitemap:oversized:${name}`);
    return entries.map((entry) => entry.loc);
  };
  for (const name of ["sitemap.xml", "sitemap-index.xml"]) {
    const urls = sitemapUrls(name);
    check(urls.length === canonicals.size && new Set(urls).size === urls.length && urls.every((url) => canonicals.has(url)), `sitemap:canonical_mismatch:${name}`);
  }
  const rss = array(xml("rss.xml").rss?.channel?.item);
  check(rss.length === pages.length && new Set(rss.map((p) => p.title)).size === pages.length, "rss:missing_or_duplicate_titles");
  check(new Set(rss.map((p) => p.description)).size === pages.length, "rss:duplicate_summaries");
  check(rss.every((item) => pages.some((p) => p.canonical === item.link && p.title === item.title && p.description === item.description)), "rss:metadata_mismatch");
  const robots = read("robots.txt").split(/\r?\n/);
  const groups = new Map();
  let agents = [];
  let hasRules = false;
  for (const raw of robots) {
    const match = raw.replace(/#.*/, "").trim().match(/^([^:]+):\s*(.*)$/);
    if (!match) continue;
    const [, directive, value] = match;
    if (directive.toLowerCase() === "user-agent") {
      if (hasRules) agents = [];
      agents.push(value.toLowerCase());
      hasRules = false;
    } else if (/^(allow|disallow)$/i.test(directive)) {
      for (const agent of agents) {
        if (!groups.has(agent)) groups.set(agent, []);
        groups.get(agent).push([directive.toLowerCase(), value]);
      }
      hasRules = true;
    }
  }
  for (const [bots, policy] of [[SEARCH_BOTS, "allow"], [TRAINING_BOTS, "disallow"]]) {
    for (const bot of bots) {
      const rules = groups.get(bot.toLowerCase()) ?? [];
      check(rules.some(([directive, value]) => directive === policy && value === "/")
        && !rules.some(([directive, value]) => directive !== policy && value === "/"), `robots:wrong_policy:${bot}`);
    }
  }
  check(read("robots.txt").includes(`Sitemap: ${SITE_ROOT}/sitemap.xml`), "robots:sitemap");
  check(read(`${INDEXNOW_KEY}.txt`).trim() === INDEXNOW_KEY, "indexnow:key");
  const index = json("docs-index.json");
  check(index.pages.length === pages.length && index.pages.every((p) => pages.some((page) => page.canonical === p.url && page.title === p.title && page.description === p.description)), "api:index_mismatch");
  const openapi = json("openapi.json");
  check(openapi.openapi === "3.1.0" && openapi.servers[0].url === SITE_ROOT && openapi.security.length === 0, "openapi:metadata");
  check(Object.keys(openapi.paths).length === 1 && Object.keys(openapi.paths["/docs-index.json"]).join() === "get"
    && openapi.paths["/docs-index.json"].get.responses["200"].content["application/json"].schema.type === "object", "openapi:only_real_read_endpoint");
  const catalog = json(".well-known/api-catalog");
  check(catalog.linkset[0].anchor === `${SITE_ROOT}/.well-known/api-catalog`
    && catalog.linkset[0].item[0].href === `${SITE_ROOT}/docs-index.json`
    && catalog.linkset[1].anchor === `${SITE_ROOT}/docs-index.json`
    && catalog.linkset[1]["service-desc"][0].href === `${SITE_ROOT}/openapi.json`, "catalog:invalid_linkset");
  check(/^# auth\.md$/m.test(read("auth.md")) && /\banonymous\b/.test(read("auth.md")) && /\bGET\b/.test(read("auth.md")), "auth:incomplete");
  for (const resource of ["llms.txt", "llms-full.txt"]) {
    const corpus = read(resource);
    check(/limitations/i.test(corpus) && /not a.*guarantee/i.test(corpus), `corpus:missing_limitations:${resource}`);
    for (const page of pages) check(corpus.includes(page.canonical), `corpus:missing_page:${resource}:${page.canonical}`);
  }
  for (const page of index.pages) {
    const local = resolveLocal(page.markdown);
    check(local && existsSync(local.file), `markdown:missing:${page.markdown}`);
    if (local && existsSync(local.file)) {
      const md = readFileSync(local.file, "utf8");
      check(!/<(?:script|style|nav|footer)\b|import\.meta|<MotionLanding/i.test(md), `markdown:noise:${page.url}`);
      check(md.includes(page.url) && /^# /m.test(md), `markdown:content_missing:${page.url}`);
    }
  }
  return { failures, pages, index };
}

export async function validateLive(baseUrl, { dist = defaultDist, pages, index } = {}) {
  const failures = [];
  const target = new URL(`${baseUrl.replace(/\/$/, "")}/`);
  const localUrl = (canonical) => new URL(new URL(canonical).pathname.slice(BASE_PATH.length).replace(/^\/+/, ""), target);
  const request = async (url, accept) => {
    const response = await fetch(url, { headers: { Accept: accept }, redirect: "manual", signal: AbortSignal.timeout(20000) });
    const body = await response.text();
    return { response, body };
  };
  const check = (condition, name) => { if (!condition) failures.push(name); };
  const vary = (response) => {
    const tokens = (response.headers.get("vary") ?? "").toLowerCase().split(",").map((v) => v.trim());
    return tokens.includes("accept") && tokens.includes("accept-encoding") && /\bno-store\b/i.test(response.headers.get("cdn-cache-control") ?? "");
  };
  for (const page of pages) {
    for (const accept of ["text/markdown, text/html;q=0.8", "text/html", "text/markdown", "text/html, text/markdown;q=0.5"]) {
      const { response, body } = await request(localUrl(page.canonical), accept);
      const markdown = accept.startsWith("text/markdown");
      check(response.status === 200, `http:page_status:${page.canonical}:${response.status}`);
      check(vary(response), `http:cache_isolation:${page.canonical}:${accept}`);
      check(response.headers.get("content-type")?.startsWith(markdown ? "text/markdown; charset=utf-8" : "text/html"), `http:content_type:${page.canonical}:${accept}`);
      if (markdown) {
        const mirror = index.pages.find((entry) => entry.url === page.canonical).markdown;
        const expected = readFileSync(path.join(dist, new URL(mirror).pathname.slice(BASE_PATH.length + 1)), "utf8");
        check(body === expected, `http:markdown_contamination:${page.canonical}`);
      } else failures.push(...validatePage(body, page.canonical).map((name) => `http:${page.canonical}:${name}`));
    }
  }
  for (const [resource, type] of [
    [".well-known/api-catalog", "application/linkset+json"], ["openapi.json", "application/openapi+json"],
    ["docs-index.json", "application/json"], ["auth.md", "text/markdown"], ["llms.txt", "text/plain"],
    ["llms-full.txt", "text/plain"], ["robots.txt", "text/plain"], ["sitemap.xml", "application/xml"],
    ["rss.xml", "application/xml"], [`${INDEXNOW_KEY}.txt`, "text/plain"],
  ]) {
    const { response, body } = await request(new URL(resource, target), type);
    check(response.status === 200 && response.headers.get("content-type")?.startsWith(type), `http:discovery:${resource}:${response.status}`);
    check(body === readFileSync(path.join(dist, resource), "utf8"), `http:stale_resource:${resource}`);
    if (resource !== "docs-index.json" && !resource.endsWith(".xml")) {
      const root = await request(new URL(`/${resource}`, target), type);
      check(root.response.status === 200 && root.response.headers.get("content-type")?.startsWith(type) && root.body === body, `http:root_discovery:${resource}`);
    }
  }
  const home = await request(target, "text/html");
  for (const value of discoveryLink().split(", ")) {
    check(home.response.headers.get("link")?.includes(value), `http:missing_link_header:${value}`);
  }
  const resources = new Set();
  for (const page of pages) {
    for (const node of elements(page.doc, (n) => ["a", "img", "link", "script"].includes(n.tagName))) {
      const href = attr(node, ["img", "script"].includes(node.tagName) ? "src" : "href");
      if (!href) continue;
      const url = new URL(href, page.canonical);
      if (url.origin !== SITE_ORIGIN || !url.pathname.startsWith(`${BASE_PATH}/`) || pages.some((p) => p.canonical === `${url.origin}${url.pathname}`)) continue;
      url.hash = "";
      resources.add(url.href);
    }
  }
  for (const href of resources) {
    const response = await fetch(localUrl(href), { redirect: "manual", signal: AbortSignal.timeout(20000) });
    const bytes = Buffer.from(await response.arrayBuffer());
    const relative = decodeURIComponent(new URL(href).pathname.slice(BASE_PATH.length + 1));
    const file = path.join(dist, relative.endsWith("/") ? `${relative}index.html` : relative);
    check(response.status === 200 && existsSync(file) && bytes.equals(readFileSync(file)), `http:broken_or_stale_internal_asset:${href}`);
  }
  for (const route of ["ko/", "en/", "__missing_agent_preflight__/"]) {
    const { response, body } = await request(new URL(route, target), "text/html");
    check(response.status === 404 && (/\bnoindex\b/i.test(response.headers.get("x-robots-tag") ?? "") || meta(htmlDocument(body), "robots").some((value) => /\bnoindex\b/.test(value))), `http:not_honest_404:${route}`);
  }
  return failures;
}

async function main() {
  const result = await validateArtifacts();
  const urlAt = process.argv.indexOf("--url");
  if (urlAt !== -1) {
    const url = process.argv[urlAt + 1];
    if (!url) throw new Error("--url requires the deployed site URL, including its base path");
    result.failures.push(...await validateLive(url, result));
  }
  for (const failure of result.failures.slice(0, 40)) console.error(`  - ${failure}`);
  if (result.failures.length > 40) console.error(`  ... ${result.failures.length - 40} further errors (use the exported validators for full results)`);
  console.log(`[check-seo-geo] ${result.pages.length} indexable HTML pages; ${result.failures.length} errors${urlAt === -1 ? " (build artifacts only; HTTP and CWV not measured)" : " (artifacts + strict HTTP gate; CWV requires browser measurement)"}`);
  if (result.failures.length) process.exitCode = 1;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => { console.error(`[check-seo-geo] ${error.stack}`); process.exitCode = 1; });
}

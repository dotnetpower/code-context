import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { parse } from "parse5";

export const SITE_ORIGIN = new URL(process.env.SITE_URL ?? "https://dotnetpower.github.io").origin;
export const BASE_PATH = `/${process.env.BASE_PATH ?? "code-context"}`.replace(/\/+/g, "/").replace(/\/$/, "");
export const SITE_ROOT = `${SITE_ORIGIN}${BASE_PATH}`;
export const INDEXNOW_KEY = "c7d4e9a1b2f84c6093e5a7d21f0b8e4c";
export const SEARCH_BOTS = ["Googlebot", "bingbot", "Yeti", "Daum", "DuckDuckBot", "OAI-SearchBot", "Claude-SearchBot", "PerplexityBot", "ChatGPT-User", "Claude-User", "Perplexity-User"];
export const TRAINING_BOTS = ["GPTBot", "ClaudeBot", "Google-Extended", "Applebot-Extended", "CCBot", "Claude-Web", "anthropic-ai", "cohere-ai", "Bytespider"];
export const LIMITATIONS = "Measurements are from the repository's published benchmark fixtures, not a performance or billing guarantee. Hardware, workload, cache state and model pricing change results. This documentation site cannot search or read a visitor's local source code. Indexing is local; explicitly requested HTTP probes contact their supplied URLs.";

export function walkFiles(dir) {
  return readdirSync(dir).sort().flatMap((name) => {
    const full = path.join(dir, name);
    return statSync(full).isDirectory() ? walkFiles(full) : [full];
  });
}

export function htmlDocument(html, options = {}) {
  return parse(html, options);
}

export function elements(node, predicate) {
  const found = [];
  function visit(current) {
    if (current.tagName && predicate(current)) found.push(current);
    for (const child of current.childNodes ?? []) visit(child);
  }
  visit(node);
  return found;
}

export const attr = (node, name) => node?.attrs?.find((item) => item.name === name)?.value;
export const text = (node) => node?.nodeName === "#text"
  ? node.value
  : (node?.childNodes ?? []).map(text).join("");
export const meta = (doc, key) => elements(doc, (n) => n.tagName === "meta" && (attr(n, "name") === key || attr(n, "property") === key)).map((n) => attr(n, "content"));
export const links = (doc, rel) => elements(doc, (n) => n.tagName === "link" && attr(n, "rel")?.split(/\s+/).includes(rel));
export const xmlEscape = (value) => String(value).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&apos;" })[c]);

export function loadPages(dist) {
  return walkFiles(dist).filter((file) => file.endsWith(".html")).map((file) => {
    const html = readFileSync(file, "utf8");
    const doc = htmlDocument(html);
    return {
      file, html, doc,
      relative: path.relative(dist, file).split(path.sep).join("/"),
      title: text(elements(doc, (n) => n.tagName === "title")[0]),
      description: meta(doc, "description")[0],
      canonical: attr(links(doc, "canonical")[0], "href"),
      indexable: !meta(doc, "robots").some((value) => /\bnoindex\b/i.test(value)),
    };
  });
}

export function markdownForPage(page) {
  const ignored = new Set(["script", "style", "nav", "footer", "button", "svg", "canvas", "form", "input", "select"]);
  const render = (node) => {
    if (node.nodeName === "#text") return node.value.replace(/\s+/g, " ");
    const tag = node.tagName;
    if (ignored.has(tag) || attr(node, "aria-hidden") === "true") return "";
    const children = () => (node.childNodes ?? []).map(render).join("");
    if (tag === "pre") {
      const code = text(node).trimEnd();
      const fence = "`".repeat(Math.max(3, ...[...code.matchAll(/`+/g)].map((m) => m[0].length + 1)));
      return `\n\n${fence}\n${code}\n${fence}\n\n`;
    }
    if (tag === "code") return `\`${text(node).replace(/\s+/g, " ")}\``;
    if (/^h[1-6]$/.test(tag)) return `\n\n${"#".repeat(Number(tag[1]))} ${children().trim()}\n\n`;
    if (tag === "a") {
      const label = children().trim();
      return label && attr(node, "href") ? `[${label}](${new URL(attr(node, "href"), page.canonical).href})` : label;
    }
    if (tag === "img") return `![${attr(node, "alt")}](${new URL(attr(node, "src"), page.canonical).href})`;
    if (tag === "li") return `\n- ${children().trim()}`;
    if (tag === "dt") return `\n\n**${children().trim()}**: `;
    if (tag === "dd") return `${children().trim()}\n\n`;
    if (tag === "tr") return `\n| ${elements(node, (n) => ["td", "th"].includes(n.tagName)).map((n) => render(n).trim().replace(/\|/g, "\\|")).join(" | ")} |`;
    if (tag === "thead") {
      const columns = elements(node, (n) => n.tagName === "th").length;
      return `${children()}\n| ${Array(columns).fill("---").join(" | ")} |`;
    }
    if (tag === "br") return "\n";
    if (["p", "section", "article", "div", "details", "summary", "table", "ul", "ol", "dl", "figure"].includes(tag)) return `\n\n${children().trim()}\n\n`;
    return children();
  };
  const main = elements(page.doc, (n) => n.tagName === "main")[0];
  if (!main) throw new Error(`Missing main content: ${page.relative}`);
  return `Canonical: ${page.canonical}\n\n${render(main).trim().replace(/\n{3,}/g, "\n\n")}\n\n## Sources and limitations\n\n${LIMITATIONS}\n`;
}

export function prefersMarkdown(accept = "") {
  const ranges = accept.toLowerCase().split(",").map((part, order) => {
    const [type, ...parameters] = part.trim().split(";");
    const raw = parameters.map((p) => p.trim()).find((p) => p.startsWith("q="))?.slice(2);
    const q = raw === undefined ? 1 : Number(raw);
    return { type, q: Number.isFinite(q) && q >= 0 && q <= 1 ? q : 0, order };
  });
  const score = (type) => {
    const exact = ranges.find((r) => r.type === type);
    if (exact) return exact;
    return ranges.find((r) => r.type === "text/*") ?? ranges.find((r) => r.type === "*/*") ?? { q: 0, order: Infinity };
  };
  const md = score("text/markdown");
  const html = score("text/html");
  return ranges.some((r) => r.type === "text/markdown") && md.q > 0 && (md.q > html.q || (md.q === html.q && md.order < html.order));
}

export function discoveryLink() {
  return [
    ["/.well-known/api-catalog", "api-catalog", "application/linkset+json"],
    ["/openapi.json", "service-desc", "application/openapi+json"],
    ["/llms.txt", "service-doc", "text/plain"],
    ["/auth.md", "describedby", "text/markdown"],
  ].map(([href, rel, type]) => `<${href}>; rel="${rel}"; type="${type}"`).join(", ");
}

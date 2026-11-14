import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { BASE_PATH, discoveryLink, INDEXNOW_KEY, prefersMarkdown } from "./site-contract.mjs";

const defaultDist = path.resolve(fileURLToPath(new URL("../dist/", import.meta.url)));
const rootAliases = new Set(["/robots.txt", "/llms.txt", "/llms-full.txt", "/openapi.json", "/auth.md", "/.well-known/api-catalog", `/${INDEXNOW_KEY}.txt`]);
const mime = {
  ".html": "text/html", ".md": "text/markdown", ".txt": "text/plain",
  ".json": "application/json", ".xml": "application/xml", ".css": "text/css",
  ".js": "text/javascript", ".svg": "image/svg+xml", ".png": "image/png",
  ".webp": "image/webp", ".jpg": "image/jpeg", ".woff2": "font/woff2",
  ".csv": "text/csv", ".ico": "image/x-icon",
};

async function isFile(file) {
  try {
    return (await stat(file)).isFile();
  } catch (error) {
    if (error.code === "ENOENT" || error.code === "ENOTDIR") return false;
    throw error;
  }
}

export function createAgentServer({ dist = defaultDist } = {}) {
  dist = path.resolve(dist);
  return createServer(async (req, res) => {
    res.setHeader("Vary", "Accept, Accept-Encoding");
    res.setHeader("CDN-Cache-Control", "no-store");
    res.setHeader("Cache-Control", "no-store");
    res.setHeader("X-Content-Type-Options", "nosniff");
    const send = (status, type, body) => {
      res.writeHead(status, { "Content-Type": type });
      res.end(req.method === "HEAD" ? undefined : body);
    };
    try {
      if (!["GET", "HEAD"].includes(req.method)) {
        res.setHeader("Allow", "GET, HEAD");
        return send(405, "text/plain; charset=utf-8", "Read-only service: use GET or HEAD.\n");
      }
      const url = new URL(req.url, "http://localhost");
      let pathname;
      try {
        pathname = decodeURIComponent(url.pathname);
      } catch {
        return send(400, "text/plain; charset=utf-8", "Invalid URL encoding.\n");
      }
      const underBase = !BASE_PATH || pathname === BASE_PATH || pathname.startsWith(`${BASE_PATH}/`);
      let relative = underBase ? pathname.slice(BASE_PATH.length) : rootAliases.has(pathname) ? pathname : null;
      if (relative?.includes("\0") || relative?.split("/").includes("..")) {
        return send(400, "text/plain; charset=utf-8", "Invalid resource path.\n");
      }
      let file = relative === null ? null : path.resolve(dist, `.${relative || "/"}`);
      if (file && file !== dist && !file.startsWith(`${dist}${path.sep}`)) file = null;
      if (file && pathname.endsWith("/index.html") && await isFile(file)) {
        res.setHeader("Location", `${url.pathname.slice(0, -"index.html".length)}${url.search}`);
        return send(301, "text/plain; charset=utf-8", "Use the canonical directory URL.\n");
      }
      const index = file ? path.join(file, "index.html") : null;
      if (index && await isFile(index)) {
        if (!pathname.endsWith("/")) {
          res.setHeader("Location", `${url.pathname}/${url.search}`);
          return send(301, "text/plain; charset=utf-8", "Use the canonical trailing-slash URL.\n");
        }
        file = index;
      }
      if (!file || !await isFile(file) || path.relative(dist, file) === "404.html") {
        res.setHeader("X-Robots-Tag", "noindex");
        return send(404, "text/html; charset=utf-8", await readFile(path.join(dist, "404.html")));
      }
      if (pathname === `${BASE_PATH}/`) res.setHeader("Link", discoveryLink());
      if (file.endsWith(".html") && prefersMarkdown(req.headers.accept)) {
        relative = path.relative(dist, file).split(path.sep).join("/");
        const markdown = relative === "index.html" ? "index.md" : relative.replace(/\/index\.html$/, ".md");
        return send(200, "text/markdown; charset=utf-8", await readFile(path.join(dist, markdown)));
      }
      const type = file.endsWith("/.well-known/api-catalog") ? 'application/linkset+json; profile="https://www.rfc-editor.org/info/rfc9727"'
        : file.endsWith("/openapi.json") ? "application/openapi+json"
          : mime[path.extname(file)] ?? "application/octet-stream";
      return send(200, `${type}${/^(text\/|application\/(json|xml|.*\+json))/.test(type) ? "; charset=utf-8" : ""}`, await readFile(file));
    } catch (error) {
      console.error("[serve-agents] request failed:", error);
      if (!res.headersSent) send(500, "text/plain; charset=utf-8", "Resource could not be served.\n");
      else res.destroy(error);
    }
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const host = process.env.HOST ?? "127.0.0.1";
  const port = Number(process.env.PORT ?? 4321);
  const server = createAgentServer();
  server.on("error", (error) => { console.error("[serve-agents]", error); process.exitCode = 1; });
  server.listen(port, host, () => console.log(`[serve-agents] listening on http://${host}:${server.address().port}${BASE_PATH}/`));
}

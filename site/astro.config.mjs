// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

// GitHub Pages project site: https://dotnetpower.github.io/code-context/
// Override with SITE_URL / BASE_PATH for custom domains (e.g. SITE_URL=https://cctx.dev BASE_PATH=/).
const SITE_URL = process.env.SITE_URL ?? "https://dotnetpower.github.io";
const IS_BUILD = process.env.NODE_ENV === "production" || process.argv.includes("build") || process.argv.includes("preview");
const BASE_PATH = process.env.BASE_PATH ?? (IS_BUILD ? "/code-context" : "/");

/** Open external document content links in a new tab while keeping internal links in the same tab. */
function rehypeDocumentLinksNewTab() {
  const isExternalHref = (/** @type {string} */ href) =>
    /^https?:\/\//i.test(href) && !href.startsWith("https://dotnetpower.github.io/code-context");
  const visit = (/** @type {any} */ node) => {
    if (!node || typeof node !== "object") return;
    if (node.type === "element" && node.tagName === "a") {
      const props = (node.properties ??= {});
      const href = String(props.href ?? "").trim();
      if (isExternalHref(href)) {
        props.target = "_blank";
        props.rel = "noopener noreferrer";
      }
    } else if (node.type === "raw" && typeof node.value === "string" && node.value.includes("<a ")) {
      node.value = node.value.replace(
        /<a\s+(?![^>]*\btarget=)([^>]*\bhref=["'](https?:\/\/(?!dotnetpower\.github\.io\/code-context)[^"']+)["'][^>]*)>/gi,
        '<a target="_blank" rel="noopener noreferrer" $1>',
      );
    }
    if (Array.isArray(node.children)) {
      for (const child of node.children) visit(child);
    }
  };
  return (/** @type {any} */ tree) => visit(tree);
}

export default defineConfig({
  site: SITE_URL,
  base: BASE_PATH,
  trailingSlash: "ignore",
  markdown: {
    rehypePlugins: [rehypeDocumentLinksNewTab],
  },
  integrations: [
    starlight({
      title: "Code Context (cctx)",
      logo: {
        src: "./public/favicon.svg",
        replacesTitle: false,
      },
      favicon: "/favicon.svg",
      description:
        "Fast local file discovery, trigram-indexed source search, and block-clamped context cache for AI coding agents — Native VS Code Language Model Tools & MCP server in Rust.",
      defaultLocale: "root",
      locales: {
        root: { label: "English", lang: "en" },
      },
      customCss: ["./src/styles/custom.css"],
      components: {
        Head: "./src/components/Head.astro",
      },
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/dotnetpower/code-context",
        },
      ],
      editLink: {
        baseUrl: "https://github.com/dotnetpower/code-context/edit/main/site/",
      },
      sidebar: [
        {
          label: "Start Here",
          items: [
            { label: "Installation & Platforms", slug: "installation" },
            { label: "Quickstart & Agent Workflow", slug: "quickstart" },
          ],
        },
        {
          label: "Tools & Primitives",
          items: [
            { label: "Native VS Code & MCP Tools", slug: "tools" },
            { label: "Probes (pctx), Journal & Formula", slug: "probes-and-journal" },
          ],
        },
        {
          label: "Evidence & Internals",
          items: [
            { label: "Benchmarks & Token Suites", slug: "benchmarks" },
            { label: "Architecture, Memory & Privacy", slug: "architecture" },
            { label: "FAQ", slug: "faq" },
          ],
        },
      ],
    }),
  ],
});

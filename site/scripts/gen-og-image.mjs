// gen-og-image.mjs — Sync static benchmark figures into site/public/benchmarks/
// and render the 1200x630 Open Graph / Twitter cover PNG via sharp.

import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const siteDir = path.resolve(scriptDir, "..");
const repoRoot = path.resolve(siteDir, "..");
const publicDir = path.join(siteDir, "public");
const publicBenchDir = path.join(publicDir, "benchmarks");

mkdirSync(publicBenchDir, { recursive: true });

// 1. Copy benchmark figures & root llms.txt into site/public before astro build
const benchAssets = [
  "latency-speedup.svg",
  "latency-speedup.png",
  "cold-start.svg",
  "summary.csv",
];
for (const file of benchAssets) {
  const src = path.join(repoRoot, "benchmarks", "results", "aks-store-demo", file);
  if (existsSync(src)) {
    copyFileSync(src, path.join(publicBenchDir, file));
  }
}

const rootLlms = path.join(repoRoot, "llms.txt");
if (existsSync(rootLlms)) {
  writeFileSync(path.join(publicDir, "llms.txt"), readFileSync(rootLlms, "utf8"), "utf8");
}

// 2. Render 1200x630 og-cover.png
const WIDTH = 1200;
const HEIGHT = 630;
const OUT = path.join(publicDir, "og-cover.png");

const svg = `<svg width="${WIDTH}" height="${HEIGHT}" viewBox="0 0 ${WIDTH} ${HEIGHT}" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <linearGradient id="bg" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#090e1c"/>
      <stop offset="55%" stop-color="#101933"/>
      <stop offset="100%" stop-color="#0c2231"/>
    </linearGradient>
    <radialGradient id="glowA" cx="20%" cy="24%" r="54%">
      <stop offset="0%" stop-color="#3b5bdb" stop-opacity="0.36"/>
      <stop offset="100%" stop-color="#3b5bdb" stop-opacity="0"/>
    </radialGradient>
    <radialGradient id="glowB" cx="82%" cy="74%" r="50%">
      <stop offset="0%" stop-color="#54ead2" stop-opacity="0.24"/>
      <stop offset="100%" stop-color="#54ead2" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="accentGrad" x1="0%" y1="0%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#91a7ff"/>
      <stop offset="50%" stop-color="#66d9e8"/>
      <stop offset="100%" stop-color="#54ead2"/>
    </linearGradient>
    <pattern id="grid" width="36" height="36" patternUnits="userSpaceOnUse">
      <path d="M 36 0 L 0 0 0 36" fill="none" stroke="#ffffff" stroke-opacity="0.045" stroke-width="1"/>
    </pattern>
  </defs>

  <rect width="${WIDTH}" height="${HEIGHT}" fill="url(#bg)"/>
  <rect width="${WIDTH}" height="${HEIGHT}" fill="url(#glowA)"/>
  <rect width="${WIDTH}" height="${HEIGHT}" fill="url(#glowB)"/>
  <rect width="${WIDTH}" height="${HEIGHT}" fill="url(#grid)"/>
  <rect x="0" y="0" width="${WIDTH}" height="6" fill="url(#accentGrad)"/>

  <!-- Eyebrow badge -->
  <rect x="68" y="56" width="690" height="36" rx="18" fill="#162344" stroke="#3b5bdb" stroke-width="1.3"/>
  <circle cx="92" cy="74" r="5" fill="#54ead2"/>
  <text x="108" y="79" font-family="'DejaVu Sans Mono', monospace" font-size="13.5" font-weight="700" letter-spacing="1.4" fill="#dbe7ff">
    LOCAL RUST CONTEXT ENGINE · NATIVE VS CODE TOOLS + MCP
  </text>

  <!-- Main Headline -->
  <text x="68" y="168" font-family="'DejaVu Sans', Arial, sans-serif" font-size="72" font-weight="700" letter-spacing="-1.5" fill="#ffffff">
    Code Context (cctx)
  </text>
  <text x="68" y="226" font-family="'DejaVu Sans', Arial, sans-serif" font-size="33" font-weight="700" fill="url(#accentGrad)">
    Find the signal. Clamp the noise in 0.23 ms.
  </text>
  <text x="68" y="272" font-family="'DejaVu Sans', Arial, sans-serif" font-size="19.5" fill="#b8c7e4">
    Shared Rust trigram index, AST block clamping, rich #toc/#slide anchors, pctx probes &amp; cctx journal.
  </text>

  <!-- 3 Measured Proof Cards -->
  <g transform="translate(68, 318)">
    <rect x="0" y="0" width="340" height="224" rx="16" fill="#0f182e" stroke="#2c4470" stroke-width="1.5"/>
    <text x="24" y="38" font-family="'DejaVu Sans Mono', monospace" font-size="13" font-weight="700" fill="#54ead2">
      WARM RETRIEVAL (8,960 PAIRS)
    </text>
    <text x="24" y="92" font-family="'DejaVu Sans Mono', monospace" font-size="44" font-weight="700" fill="#ffffff">
      17.22x
    </text>
    <text x="24" y="130" font-family="'DejaVu Sans', sans-serif" font-size="16" fill="#c3d2ee">
      3.996 ms -&gt; 0.232 ms mean
    </text>
    <text x="24" y="158" font-family="'DejaVu Sans', sans-serif" font-size="15" fill="#8ea4ca">
      22.77x literal · 21.39x context
    </text>
    <text x="24" y="186" font-family="'DejaVu Sans', sans-serif" font-size="15" fill="#8ea4ca">
      Nonblocking CACHE_MISS fallback
    </text>

    <rect x="362" y="0" width="340" height="224" rx="16" fill="#0f182e" stroke="#2c4470" stroke-width="1.5"/>
    <text x="386" y="38" font-family="'DejaVu Sans Mono', monospace" font-size="13" font-weight="700" fill="#91a7ff">
      LOOKUP PAYLOAD TOKENS
    </text>
    <text x="386" y="92" font-family="'DejaVu Sans Mono', monospace" font-size="42" font-weight="700" fill="#ffffff">
      -73% to -91%
    </text>
    <text x="386" y="130" font-family="'DejaVu Sans', sans-serif" font-size="16" fill="#c3d2ee">
      1 call AST block-clamped context
    </text>
    <text x="386" y="158" font-family="'DejaVu Sans', sans-serif" font-size="15" fill="#8ea4ca">
      -86% to -98.9% on .pptx/.docx #toc
    </text>
    <text x="386" y="186" font-family="'DejaVu Sans', sans-serif" font-size="15" fill="#8ea4ca">
      -99.94% on pctx JSON probes
    </text>

    <rect x="724" y="0" width="340" height="224" rx="16" fill="#0f182e" stroke="#2c4470" stroke-width="1.5"/>
    <text x="748" y="38" font-family="'DejaVu Sans Mono', monospace" font-size="13" font-weight="700" fill="#ffd43b">
      COPILOT A/B AGENT TRIALS
    </text>
    <text x="748" y="92" font-family="'DejaVu Sans Mono', monospace" font-size="44" font-weight="700" fill="#ffffff">
      -24.7%
    </text>
    <text x="748" y="130" font-family="'DejaVu Sans', sans-serif" font-size="16" fill="#c3d2ee">
      Faster task completion wall time
    </text>
    <text x="748" y="158" font-family="'DejaVu Sans', sans-serif" font-size="15" fill="#8ea4ca">
      -61.9% tool calls · -25.4% AIU cost
    </text>
    <text x="748" y="186" font-family="'DejaVu Sans', sans-serif" font-size="15" fill="#8ea4ca">
      Linux · Native Windows · WSL · SSH
    </text>
  </g>
</svg>`;

await sharp(Buffer.from(svg)).png({ compressionLevel: 9 }).toFile(OUT);
console.log(`[gen-og] wrote ${OUT}`);

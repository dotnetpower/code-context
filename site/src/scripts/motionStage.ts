/**
 * motionStage.ts — Pure-function-of-time frame(t) AI Motion-Graphics Engine for Code Context (cctx).
 *
 * Principles:
 * 1. Fixed aspect-ratio stage (zero DOM layout shift, zero getBoundingClientRect() in rAF).
 * 2. Dual-state pixel-registered renderer (`paintStateA` vs `paintStateB`) composited via an
 *    offscreen feathered diagonal wipe blade with minimum-jerk motion (`10p^3 - 15p^4 + 6p^5`).
 * 3. Closed-form damped springs, camera impact punch, shockwave ring, and diagonal light glint.
 * 4. Seamless C1 loop boundary: `frame(10.0)` is pixel-identical to `frame(0.0)`.
 */

export const HERO_DURATION = 10.0;

export const clamp = (v: number, a = 0, b = 1) => (v < a ? a : v > b ? b : v);
export const lerp = (a: number, b: number, k: number) => a + (b - a) * k;
export const smooth = (v: number) => {
  const x = clamp(v);
  return x * x * (3 - 2 * x);
};
export const ramp = (t: number, a: number, b: number) => smooth((t - a) / Math.max(1e-6, b - a));

/** Minimum-jerk trajectory s(p) = 10p^3 - 15p^4 + 6p^5: zero velocity & acceleration at both ends. */
export const minJerk = (p: number) => {
  const x = clamp(p);
  return x * x * x * (10 - 15 * x + 6 * x * x);
};

/** Closed-form underdamped spring (0 -> 1 with tactile overshoot). */
export function spring(p: number, omega = 15, zeta = 0.44): number {
  const x = clamp(p, 0, 4);
  if (x <= 0) return 0;
  const wd = omega * Math.sqrt(Math.max(1e-4, 1 - zeta * zeta));
  return (
    1 -
    Math.exp(-zeta * omega * x) *
      (Math.cos(wd * x) + ((zeta * omega) / wd) * Math.sin(wd * x))
  );
}

const MONO_FONT = `"JetBrains Mono", "SFMono-Regular", Consolas, monospace`;
const SANS_FONT = `"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif`;

export function initCctxMotion(): void {
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  initQuickInstallTabs();
  initHeroStage(reducedMotion);
  initEnginePipelineStage(reducedMotion);
}

/* ==========================================================================
   0. Hero Quick-Install OS Switcher & 1-Click Copy
   ========================================================================== */
function initQuickInstallTabs(): void {
  const snippets: Record<string, string> = {
    linux:
      "git clone https://github.com/dotnetpower/code-context.git && cd code-context && bash scripts/install.sh",
    windows:
      "cargo build --locked --release; copy target\\release\\cctx.exe $HOME\\.local\\bin\\cctx.exe",
    vscode: "code --install-extension dotnetpower.code-context-native",
    mcp: "cctx --root /path/to/project shared-mcp",
  };

  const tabs = document.querySelectorAll<HTMLButtonElement>("[data-os-tab]");
  const codeEl = document.getElementById("cctx-quick-code");
  const copyBtn = document.getElementById("cctx-quick-copy") as HTMLButtonElement | null;

  tabs.forEach((btn) => {
    btn.addEventListener("click", () => {
      const key = btn.dataset.osTab || "linux";
      tabs.forEach((b) => b.setAttribute("aria-selected", String(b === btn)));
      if (codeEl && snippets[key]) {
        codeEl.textContent = snippets[key];
      }
    });
  });

  if (copyBtn && codeEl) {
    copyBtn.addEventListener("click", async () => {
      const text = codeEl.textContent?.trim() || "";
      try {
        await navigator.clipboard.writeText(text);
        const prev = copyBtn.textContent;
        copyBtn.textContent = "Copied ✓";
        setTimeout(() => {
          copyBtn.textContent = prev || "Copy";
        }, 1400);
      } catch {
        copyBtn.textContent = "Select & Copy";
      }
    });
  }
}

/* ==========================================================================
   1. ACT 1 — Unified Canvas Cinema Film (`frame(t)`, `t` in [0, 10.0])
   ========================================================================== */
interface StageMetrics {
  w: number;
  h: number;
  dpr: number;
  compact: boolean;
}

const CODE_TARGET_LINES: Array<{ ln: string; parts: Array<[string, string]> }> = [
  {
    ln: "142",
    parts: [
      ["pub fn ", "#91a7ff"],
      ["plan_query", "#54ead2"],
      ["(hir: &", "#dce7ff"],
      ["Hir", "#ffd43b"],
      [", idx: &", "#dce7ff"],
      ["Index", "#ffd43b"],
      [") -> ", "#dce7ff"],
      ["Plan", "#ffd43b"],
      [" {", "#dce7ff"],
    ],
  },
  {
    ln: "143",
    parts: [
      ["  let ", "#91a7ff"],
      ["grams = extract_folded_trigrams(hir);", "#e6eefc"],
    ],
  },
  {
    ln: "144",
    parts: [
      ["  let ", "#91a7ff"],
      ["hits = idx.intersect_postings(&grams);", "#e6eefc"],
    ],
  },
  {
    ln: "145",
    parts: [
      ["  ", "#e6eefc"],
      ["Plan", "#ffd43b"],
      ["::clamp_ast(hits, ", "#e6eefc"],
      ["Budget", "#ffd43b"],
      ["::MAX_8KIB)", "#54ead2"],
    ],
  },
  {
    ln: "146",
    parts: [
      ["}", "#dce7ff"],
    ],
  },
];

const NOISE_TOP = [
  ["137", "const FALLBACK_WINDOW: usize = 200; // unrelated slice"],
  ["138", "fn legacy_scan_line(buf: &[u8]) -> bool { !buf.is_empty() }"],
  ["139", "// ... 136 preceding lines dumped into LLM history ..."],
];

const NOISE_BOTTOM = [
  ["147", "#[cfg(test)] mod planner_fixture_tests { // leaked tests"],
  ["148", "  fn test_legacy_window() { assert_eq!(2 + 2, 4); }"],
  ["149", "// ... +53 trailing lines re-billed every agent turn ..."],
];

function drawRoundRect(
  g: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
): void {
  g.beginPath();
  g.roundRect(x, y, w, h, r);
}

function initHeroStage(reducedMotion: boolean): void {
  const cinema = document.getElementById("cctx-cinema");
  const screen = document.getElementById("cctx-stage-screen");
  const canvas = document.getElementById("cctx-hero-canvas") as HTMLCanvasElement | null;
  if (!cinema || !screen || !canvas) return;

  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  // Offscreen buffers allocated once per resize — zero per-frame allocation or DOM layout reads
  const bufA = document.createElement("canvas");
  const bufB = document.createElement("canvas");
  const ctxA = bufA.getContext("2d")!;
  const ctxB = bufB.getContext("2d")!;

  const timecodeEl = document.getElementById("cctx-timecode");
  const phaseBadgeEl = document.getElementById("cctx-phase-badge");
  const scrubRange = document.getElementById("cctx-scrub-range") as HTMLInputElement | null;
  const playBtn = document.getElementById("cctx-scrub-play") as HTMLButtonElement | null;
  const beatPills = document.querySelectorAll<HTMLButtonElement>("[data-beat-seek]");
  const modeBtns = document.querySelectorAll<HTMLButtonElement>("[data-stage-mode]");

  let m: StageMetrics = { w: 600, h: 390, dpr: 1, compact: false };
  let playing = !reducedMotion;
  let currentT = reducedMotion ? 6.8 : 0.0;
  let seekFromT = currentT;
  let seekToT: number | null = null;
  let seekElapsed = 0;
  const SEEK_DURATION = 0.52;
  let lastWallMs = performance.now();

  const resize = () => {
    const rect = screen.getBoundingClientRect();
    const w = Math.max(260, Math.round(rect.width));
    const h = Math.max(240, Math.round(rect.height));
    const dpr = Math.min(2, window.devicePixelRatio || 1);
    m = { w, h, dpr, compact: w < 460 };

    for (const c of [canvas, bufA, bufB]) {
      c.width = Math.round(w * dpr);
      c.height = Math.round(h * dpr);
    }
    renderFrame(currentT);
  };

  const setPlaying = (next: boolean) => {
    playing = next;
    if (playBtn) {
      playBtn.textContent = playing ? "Pause ❚❚" : "Play ▶";
    }
  };

  const setActiveModeBtn = (mode: string) => {
    modeBtns.forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.stageMode === mode)));
  };

  const startSmoothSeek = (target: number) => {
    seekFromT = currentT;
    seekToT = clamp(target, 0, HERO_DURATION);
    seekElapsed = 0;
    lastWallMs = performance.now();
  };

  setPlaying(playing);

  if (playBtn) {
    playBtn.addEventListener("click", () => {
      seekToT = null;
      setActiveModeBtn("film");
      setPlaying(!playing);
      lastWallMs = performance.now();
    });
  }

  if (scrubRange) {
    scrubRange.addEventListener("input", () => {
      seekToT = null;
      setPlaying(false);
      currentT = clamp(parseFloat(scrubRange.value) || 0, 0, HERO_DURATION);
      renderFrame(currentT);
    });
  }

  beatPills.forEach((pill) => {
    pill.addEventListener("click", () => {
      const target = parseFloat(pill.dataset.beatSeek || "0");
      setActiveModeBtn("film");
      startSmoothSeek(target);
    });
  });

  modeBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const mode = btn.dataset.stageMode || "film";
      setActiveModeBtn(mode);
      if (mode === "film") {
        seekToT = null;
        setPlaying(true);
        lastWallMs = performance.now();
      } else if (mode === "before") {
        setPlaying(false);
        startSmoothSeek(2.1);
      } else if (mode === "after") {
        setPlaying(false);
        startSmoothSeek(6.8);
      }
    });
  });

  /**
   * Compute shared pixel-registered layout slots so State A and State B line up to the exact pixel.
   */
  function getSlots(sm: StageMetrics) {
    const pad = sm.compact ? 11 : 16;
    const cmdY = pad;
    const cmdH = sm.compact ? 30 : 34;
    const barY = cmdY + cmdH + 7;
    const barH = sm.compact ? 25 : 28;
    const gaugeH = sm.compact ? 56 : 62;
    const gaugeY = sm.h - pad - gaugeH;
    const codeY = barY + barH + 8;
    const codeH = Math.max(120, gaugeY - codeY - 9);
    const codeX = pad;
    const codeW = sm.w - pad * 2;

    const lineH = sm.compact ? 15.5 : 17.5;
    const targetPadV = sm.compact ? 20 : 23;
    const targetH = CODE_TARGET_LINES.length * lineH + targetPadV + 8;
    const targetY = Math.round(codeY + (codeH - targetH) * 0.5);

    return { pad, cmdY, cmdH, barY, barH, codeX, codeY, codeW, codeH, targetY, targetH, lineH, gaugeY, gaugeH };
  }

  /**
   * Paint State A: Traditional multi-turn `rg + 200-line view` dump (Unclamped Noise).
   */
  function paintStateA(g: CanvasRenderingContext2D, t: number, sm: StageMetrics): void {
    const s = getSlots(sm);
    const { w, h, compact } = sm;

    g.save();
    g.scale(sm.dpr, sm.dpr);

    // Studio dark-crimson backdrop
    g.fillStyle = "#0b0d18";
    g.fillRect(0, 0, w, h);

    const heat = g.createRadialGradient(w * 0.78, h * 0.28, 10, w * 0.78, h * 0.28, w * 0.75);
    heat.addColorStop(0, "rgba(250, 82, 82, 0.16)");
    heat.addColorStop(0.55, "rgba(245, 159, 0, 0.05)");
    heat.addColorStop(1, "rgba(11, 13, 24, 0)");
    g.fillStyle = heat;
    g.fillRect(0, 0, w, h);

    // Subtle dot grid
    g.fillStyle = "rgba(255, 135, 135, 0.06)";
    const step = 22;
    for (let gx = 12; gx < w; gx += step) {
      for (let gy = 12; gy < h; gy += step) {
        g.fillRect(gx, gy, 1.2, 1.2);
      }
    }

    // 1. Top Command Bar
    drawRoundRect(g, s.pad, s.cmdY, s.codeW, s.cmdH, 8);
    g.fillStyle = "#1a1222";
    g.fill();
    g.strokeStyle = "rgba(250, 82, 82, 0.42)";
    g.lineWidth = 1;
    g.stroke();

    const floodBuild = minJerk(ramp(t, 0.0, 1.75));
    const callCount = Math.round(lerp(6, 20, floodBuild));
    const tokCount = Math.round(lerp(8900, 25133, floodBuild));
    const latMs = lerp(1.65, 3.996, floodBuild);

    g.font = `600 ${compact ? 10 : 11.5}px ${MONO_FONT}`;
    g.fillStyle = "#ffa8a8";
    g.textBaseline = "middle";
    const cmdText = compact
      ? `$ rg "plan_query" → 3× view(200L)`
      : `$ rg -n "plan_query" src/  →  3× 200-line view() dumps`;
    g.fillText(cmdText, s.pad + 10, s.cmdY + s.cmdH * 0.5);

    if (!compact) {
      const badgeW = 116;
      drawRoundRect(g, s.pad + s.codeW - badgeW - 6, s.cmdY + 6, badgeW, s.cmdH - 12, 99);
      g.fillStyle = "rgba(250, 82, 82, 0.18)";
      g.fill();
      g.font = `700 9.5px ${MONO_FONT}`;
      g.fillStyle = "#ff8787";
      g.textAlign = "center";
      g.fillText("UNCLAMPED DUMP", s.pad + s.codeW - badgeW * 0.5 - 6, s.cmdY + s.cmdH * 0.5);
      g.textAlign = "left";
    }

    // 2. Context Window Bloat Meter
    drawRoundRect(g, s.pad, s.barY, s.codeW, s.barH, 7);
    g.fillStyle = "#131324";
    g.fill();
    g.strokeStyle = "rgba(255, 135, 135, 0.25)";
    g.stroke();

    const fillFrac = lerp(0.36, 0.94, floodBuild);
    const meterGrad = g.createLinearGradient(s.pad, 0, s.pad + s.codeW * fillFrac, 0);
    meterGrad.addColorStop(0, "rgba(250, 82, 82, 0.42)");
    meterGrad.addColorStop(1, "rgba(255, 146, 43, 0.56)");
    drawRoundRect(g, s.pad + 2, s.barY + 2, (s.codeW - 4) * fillFrac, s.barH - 4, 5);
    g.fillStyle = meterGrad;
    g.fill();

    g.font = `700 ${compact ? 9.5 : 10.5}px ${MONO_FONT}`;
    g.fillStyle = "#ffe3e3";
    const bloatText = compact
      ? `CONTEXT BLOAT: ${tokCount.toLocaleString("en-US")} tok (94%)`
      : `CONTEXT WINDOW BLOAT: ${tokCount.toLocaleString("en-US")} tok · +195 unrelated lines/file`;
    g.fillText(bloatText, s.pad + 10, s.barY + s.barH * 0.5);

    // 3. Code Panel (Pixel-Registered Frame)
    drawRoundRect(g, s.codeX, s.codeY, s.codeW, s.codeH, 10);
    g.fillStyle = "#080b15";
    g.fill();
    g.strokeStyle = "rgba(250, 82, 82, 0.30)";
    g.lineWidth = 1.1;
    g.stroke();

    g.save();
    drawRoundRect(g, s.codeX, s.codeY, s.codeW, s.codeH, 10);
    g.clip();

    // Sweeping red scanner band showing repetitive line scanning
    const scanY = s.codeY + ((t * 115) % (s.codeH + 44)) - 22;
    const scanGrad = g.createLinearGradient(0, scanY - 18, 0, scanY + 18);
    scanGrad.addColorStop(0, "rgba(250, 82, 82, 0)");
    scanGrad.addColorStop(0.5, "rgba(250, 82, 82, 0.14)");
    scanGrad.addColorStop(1, "rgba(250, 82, 82, 0)");
    g.fillStyle = scanGrad;
    g.fillRect(s.codeX, scanY - 18, s.codeW, 36);

    const codeFontSize = sm.w < 360 ? 8.5 : compact ? 9.4 : 11;
    g.font = `500 ${codeFontSize}px ${MONO_FONT}`;

    // Top noisy lines above targetY
    NOISE_TOP.forEach(([ln, txt], idx) => {
      const ly = s.targetY - (NOISE_TOP.length - idx) * s.lineH + 2;
      if (ly > s.codeY + 8) {
        g.fillStyle = "#6e5064";
        g.fillText(ln, s.codeX + 10, ly);
        g.fillStyle = "#9e7689";
        g.fillText(txt, s.codeX + (compact ? 34 : 42), ly);
      }
    });

    // Buried target lines (exact same Y as State B!)
    const baseLineY = s.targetY + (compact ? 22 : 25);
    CODE_TARGET_LINES.forEach((row, idx) => {
      const ly = baseLineY + idx * s.lineH;
      g.fillStyle = "#695d78";
      g.fillText(row.ln, s.codeX + 10, ly);
      const rawLine = row.parts.map((p) => p[0]).join("");
      g.fillStyle = "#b8b0c8";
      g.fillText(rawLine, s.codeX + (compact ? 34 : 42), ly);
    });

    // Bottom noisy lines below target
    NOISE_BOTTOM.forEach(([ln, txt], idx) => {
      const ly = s.targetY + s.targetH + 6 + idx * s.lineH;
      if (ly < s.codeY + s.codeH - 8) {
        g.fillStyle = "#6e5064";
        g.fillText(ln, s.codeX + 10, ly);
        g.fillStyle = "#9e7689";
        g.fillText(txt, s.codeX + (compact ? 34 : 42), ly);
      }
    });

    // Cascading file-dump warning tags in top-right of code box
    const tagW = compact ? 118 : 148;
    const tagH = 19;
    const dumpLabels = ["view planner.rs:1..200", "view query.rs:1..200", "view mcp.rs:1..200"];
    dumpLabels.forEach((lbl, idx) => {
      const appear = idx === 0 ? 1 : spring((t - idx * 0.35) * 1.6);
      if (appear <= 0.02) return;
      const tx = s.codeX + s.codeW - tagW - 8;
      const ty = s.codeY + 8 + idx * 22 + (1 - appear) * -8;
      g.save();
      g.globalAlpha = clamp(appear, 0, 0.94);
      drawRoundRect(g, tx, ty, tagW, tagH, 5);
      g.fillStyle = "#2b1422";
      g.fill();
      g.strokeStyle = "rgba(255, 107, 107, 0.55)";
      g.stroke();
      g.font = `700 ${compact ? 8.5 : 9.2}px ${MONO_FONT}`;
      g.fillStyle = "#ffa8a8";
      g.fillText(lbl, tx + 7, ty + tagH * 0.5 + 0.5);
      g.restore();
    });

    g.restore(); // end code clip

    // 4. Bottom 3-Column Gauges (State A)
    drawTelemetryRow(g, s, sm, {
      accent: "#ff8787",
      border: "rgba(250, 82, 82, 0.34)",
      bg: "#161122",
      items: [
        {
          label: "LOOKUP PAYLOAD",
          val: `${tokCount.toLocaleString("en-US")} tok`,
          sub: compact ? "200L dumps" : "Unclamped 200L windows",
        },
        {
          label: "ROUND-TRIPS",
          val: `${callCount} calls`,
          sub: compact ? "rg + view loop" : "Multi-turn rg + view",
        },
        {
          label: "RETRIEVAL",
          val: `${latMs.toFixed(3)} ms`,
          sub: compact ? "Cold scan" : "Subprocess + scan",
        },
      ],
      glintP: -1,
    });

    g.restore();
  }

  /**
   * Paint State B: Code Context (`#cctxNativeContext`) AST Block-Clamped Hit.
   */
  function paintStateB(g: CanvasRenderingContext2D, t: number, sm: StageMetrics): void {
    const s = getSlots(sm);
    const { w, h, compact } = sm;

    // Impact punch right as optical wipe locks at t = 5.25s
    const hitAge = t - 5.25;
    const punch =
      hitAge > 0 && hitAge < 0.45
        ? Math.sin((hitAge / 0.45) * Math.PI) * Math.exp(-hitAge * 6) * 0.014
        : 0;

    g.save();
    g.scale(sm.dpr, sm.dpr);

    if (punch > 0.0002) {
      g.translate(w * 0.5, h * 0.5);
      g.scale(1 + punch, 1 + punch);
      g.translate(-w * 0.5, -h * 0.5);
    }

    // Studio deep navy-emerald backdrop
    g.fillStyle = "#070d1b";
    g.fillRect(0, 0, w, h);

    const aura = g.createRadialGradient(
      w * 0.5,
      s.targetY + s.targetH * 0.5,
      16,
      w * 0.5,
      s.targetY + s.targetH * 0.5,
      w * 0.68,
    );
    aura.addColorStop(0, "rgba(84, 234, 210, 0.18)");
    aura.addColorStop(0.5, "rgba(116, 143, 252, 0.09)");
    aura.addColorStop(1, "rgba(7, 13, 27, 0)");
    g.fillStyle = aura;
    g.fillRect(0, 0, w, h);

    // Subtle precision grid
    g.fillStyle = "rgba(84, 234, 210, 0.065)";
    const step = 22;
    for (let gx = 12; gx < w; gx += step) {
      for (let gy = 12; gy < h; gy += step) {
        g.fillRect(gx, gy, 1.2, 1.2);
      }
    }

    // 1. Top Command Bar
    drawRoundRect(g, s.pad, s.cmdY, s.codeW, s.cmdH, 8);
    g.fillStyle = "#0d1d34";
    g.fill();
    g.strokeStyle = "rgba(84, 234, 210, 0.48)";
    g.lineWidth = 1;
    g.stroke();

    g.font = `700 ${compact ? 10 : 11.5}px ${MONO_FONT}`;
    g.textBaseline = "middle";
    g.fillStyle = "#54ead2";
    const fnCall = "#cctxNativeContext";
    g.fillText(fnCall, s.pad + 10, s.cmdY + s.cmdH * 0.5);
    const fnW = g.measureText(fnCall).width;
    g.font = `500 ${compact ? 9.8 : 11.2}px ${MONO_FONT}`;
    g.fillStyle = "#dce7ff";
    g.fillText(
      compact ? `("plan_query")` : `(pattern: "plan_query", radius: 12)`,
      s.pad + 10 + fnW,
      s.cmdY + s.cmdH * 0.5,
    );

    if (!compact) {
      const badgeW = 122;
      drawRoundRect(g, s.pad + s.codeW - badgeW - 6, s.cmdY + 6, badgeW, s.cmdH - 12, 99);
      g.fillStyle = "rgba(84, 234, 210, 0.16)";
      g.fill();
      g.font = `700 9.5px ${MONO_FONT}`;
      g.fillStyle = "#54ead2";
      g.textAlign = "center";
      g.fillText("0.232 ms WARM HIT", s.pad + s.codeW - badgeW * 0.5 - 6, s.cmdY + s.cmdH * 0.5);
      g.textAlign = "left";
    }

    // 2. Trigram Posting Funnel Strip
    drawRoundRect(g, s.pad, s.barY, s.codeW, s.barH, 7);
    g.fillStyle = "#0b172c";
    g.fill();
    g.strokeStyle = "rgba(116, 143, 252, 0.32)";
    g.stroke();

    drawRoundRect(g, s.pad + 2, s.barY + 2, (s.codeW - 4) * 0.16, s.barH - 4, 5);
    g.fillStyle = "rgba(84, 234, 210, 0.24)";
    g.fill();

    g.font = `700 ${compact ? 9.3 : 10.3}px ${MONO_FONT}`;
    g.fillStyle = "#8ce9db";
    const funnelStr = compact
      ? `TRIGRAMS [pla·lan·an_] → 1 AST BLOCK (-84%)`
      : `FUNNEL: 68 files → [pla · lan · an_] → 3 hits → 1 AST block (4,031 tok)`;
    g.fillText(funnelStr, s.pad + 10, s.barY + s.barH * 0.5);

    // 3. Code Panel (Exact Pixel-Registered Frame)
    drawRoundRect(g, s.codeX, s.codeY, s.codeW, s.codeH, 10);
    g.fillStyle = "#060b16";
    g.fill();
    g.strokeStyle = "rgba(84, 234, 210, 0.34)";
    g.lineWidth = 1.1;
    g.stroke();

    const drawFoldPill = (yCenter: number, text: string) => {
      g.save();
      g.strokeStyle = "rgba(116, 143, 252, 0.22)";
      g.setLineDash([4, 4]);
      g.beginPath();
      g.moveTo(s.codeX + 14, yCenter);
      g.lineTo(s.codeX + s.codeW - 14, yCenter);
      g.stroke();
      g.setLineDash([]);

      g.font = `600 ${compact ? 8.8 : 9.6}px ${MONO_FONT}`;
      const tw = g.measureText(text).width + 18;
      drawRoundRect(g, s.codeX + (s.codeW - tw) * 0.5, yCenter - 9, tw, 18, 99);
      g.fillStyle = "#0d182d";
      g.fill();
      g.strokeStyle = "rgba(116, 143, 252, 0.35)";
      g.stroke();
      g.fillStyle = "#8da4c8";
      g.textAlign = "center";
      g.fillText(text, s.codeX + s.codeW * 0.5, yCenter + 0.5);
      g.restore();
    };

    const topSpaceCenter = (s.codeY + s.targetY) * 0.5;
    if (s.targetY - s.codeY > 22) {
      drawFoldPill(
        topSpaceCenter,
        compact ? "▸ lines 1–141 clamped out" : "▸ lines 1–141 clamped out · 0 wasted tokens",
      );
    }

    const botSpaceCenter = (s.targetY + s.targetH + s.codeY + s.codeH) * 0.5;
    if (s.codeY + s.codeH - (s.targetY + s.targetH) > 22) {
      drawFoldPill(
        botSpaceCenter,
        compact ? "▸ lines 147–310 clamped out" : "▸ lines 147–310 clamped out · tests excluded",
      );
    }

    // Clamped AST Block Card (exact targetY..targetY+targetH)
    const cardX = s.codeX + 8;
    const cardW = s.codeW - 16;
    const cardGrad = g.createLinearGradient(cardX, s.targetY, cardX + cardW, s.targetY + s.targetH);
    cardGrad.addColorStop(0, "#0d2638");
    cardGrad.addColorStop(1, "#0d1b36");
    drawRoundRect(g, cardX, s.targetY, cardW, s.targetH, 8);
    g.fillStyle = cardGrad;
    g.fill();
    g.strokeStyle = "rgba(84, 234, 210, 0.62)";
    g.lineWidth = 1.3;
    g.stroke();

    // Cyan left accent spine
    drawRoundRect(g, cardX, s.targetY, 3.5, s.targetH, 2);
    g.fillStyle = "#54ead2";
    g.fill();

    // Block header metadata
    g.font = `700 ${compact ? 8.8 : 9.8}px ${MONO_FONT}`;
    g.fillStyle = "#54ead2";
    g.fillText(
      compact ? "src/planner.rs:142–146 · AST Clamp" : "src/planner.rs:142–146 · AST Block-Clamped",
      cardX + 11,
      s.targetY + 11,
    );

    // Stamp seal ("SHA-256 OK · <8 KiB") pops with spring at t >= 5.25s
    const sealP = t >= 5.25 ? spring((t - 5.25) * 1.4) : 0.85;
    if (!compact || w >= 310) {
      const sealTxt = compact ? "SHA-256 · <8 KiB" : "SHA-256 VERIFIED · <8 KiB";
      g.save();
      const sx = cardX + cardW - 10;
      const sy = s.targetY + 11;
      g.translate(sx, sy);
      const sc = t >= 5.25 ? lerp(1.24, 1.0, clamp(sealP)) : 1.0;
      g.scale(sc, sc);
      g.font = `700 ${compact ? 8.2 : 9.0}px ${MONO_FONT}`;
      g.fillStyle = "#ffd43b";
      g.textAlign = "right";
      g.fillText(sealTxt, 0, 0);
      g.restore();
    }

    // Syntax-highlighted target lines (exact same Y as State A!)
    const codeFontSizeB = sm.w < 360 ? 8.5 : compact ? 9.4 : 11;
    g.save();
    drawRoundRect(g, cardX, s.targetY, cardW, s.targetH, 8);
    g.clip();
    g.font = `500 ${codeFontSizeB}px ${MONO_FONT}`;
    const baseLineYB = s.targetY + (compact ? 22 : 25);
    CODE_TARGET_LINES.forEach((row, idx) => {
      const ly = baseLineYB + idx * s.lineH;
      g.fillStyle = "#6282ad";
      g.fillText(row.ln, s.codeX + 14, ly);

      let cx = s.codeX + (compact ? 35 : 44);
      for (const [token, color] of row.parts) {
        g.fillStyle = color;
        g.fillText(token, cx, ly);
        cx += g.measureText(token).width;
      }
      if (idx === CODE_TARGET_LINES.length - 1) {
        g.fillStyle = "#69db7c";
        g.fillText(
          compact ? " // ← AST boundary clamped" : " // ← exact AST boundary (0 wasted lines)",
          cx,
          ly,
        );
      }
    });
    g.restore();

    // Corner AST Clamping Brackets with spring lock at t = 5.22s
    const lockSpring = t >= 5.22 ? clamp(spring((t - 5.22) * 1.35), 0, 1.2) : 0;
    const bracketPad = t >= 5.22 ? lerp(14, -2, lockSpring) : 6;
    const bx = cardX - bracketPad;
    const by = s.targetY - bracketPad * 0.55;
    const bw = cardW + bracketPad * 2;
    const bh = s.targetH + bracketPad * 1.1;
    const blen = 12;

    g.strokeStyle = "#54ead2";
    g.lineWidth = 2.2;
    g.beginPath();
    g.moveTo(bx, by + blen);
    g.lineTo(bx, by);
    g.lineTo(bx + blen, by);
    g.moveTo(bx + bw - blen, by);
    g.lineTo(bx + bw, by);
    g.lineTo(bx + bw, by + blen);
    g.moveTo(bx, by + bh - blen);
    g.lineTo(bx, by + bh);
    g.lineTo(bx + blen, by + bh);
    g.moveTo(bx + bw - blen, by + bh);
    g.lineTo(bx + bw, by + bh);
    g.lineTo(bx + bw, by + bh - blen);
    g.stroke();

    // Expanding shockwave ring on clamp lock (t in [5.25, 6.05])
    if (hitAge > 0 && hitAge < 0.8) {
      const rp = hitAge / 0.8;
      const ringAlpha = (1 - rp) * 0.55;
      const expand = easeOutCubic(rp) * 24;
      drawRoundRect(
        g,
        cardX - expand,
        s.targetY - expand * 0.6,
        cardW + expand * 2,
        s.targetH + expand * 1.2,
        12,
      );
      g.strokeStyle = `rgba(84, 234, 210, ${ringAlpha.toFixed(3)})`;
      g.lineWidth = 1.8;
      g.stroke();
    }

    // Diagonal specular light glint across clamped code card (t in [5.40, 6.45])
    const glintP = (t - 5.4) / 1.05;
    if (glintP >= 0 && glintP <= 1) {
      g.save();
      drawRoundRect(g, cardX, s.targetY, cardW, s.targetH, 8);
      g.clip();
      const gx = lerp(cardX - 120, cardX + cardW + 120, easeInOutCubic(glintP));
      const glint = g.createLinearGradient(gx - 55, s.targetY, gx + 55, s.targetY + 40);
      glint.addColorStop(0, "rgba(255, 255, 255, 0)");
      glint.addColorStop(0.5, "rgba(165, 248, 235, 0.22)");
      glint.addColorStop(1, "rgba(255, 255, 255, 0)");
      g.fillStyle = glint;
      g.fillRect(cardX, s.targetY, cardW, s.targetH);
      g.restore();
    }

    // 4. Bottom 3-Column Gauges (State B)
    drawTelemetryRow(g, s, sm, {
      accent: "#54ead2",
      border: "rgba(84, 234, 210, 0.42)",
      bg: "#0c1a30",
      items: [
        {
          label: "LOOKUP PAYLOAD",
          val: "4,031 tok",
          sub: "-84.0% tokens",
        },
        {
          label: "ROUND-TRIPS",
          val: "6 calls",
          sub: compact ? "-70% calls" : "1 call/symbol (-70%)",
        },
        {
          label: "WARM RETRIEVAL",
          val: "0.232 ms",
          sub: "17.22× faster",
        },
      ],
      glintP: (t - 5.55) / 0.95,
    });

    g.restore();
  }

  /**
   * Master frame(t) compositor:
   * - [0.00, 2.80): State A (200-line grep flood)
   * - [2.80, 5.25): Two-beat minimum-jerk optical wipe from State A -> State B
   * - [5.25, 8.80): State B (AST clamp lock + impact + glint + clean reading hold)
   * - [8.80, 10.00]: Seamless dissolve back to exact State A(0.0) pose
   */
  function renderFrame(tRaw: number): void {
    const t = ((tRaw % HERO_DURATION) + HERO_DURATION) % HERO_DURATION;
    const { w, h, dpr, compact } = m;

    if (timecodeEl) {
      timecodeEl.textContent = `t = ${t.toFixed(2).padStart(5, "0")}s`;
    }
    if (scrubRange && document.activeElement !== scrubRange) {
      scrubRange.value = t.toFixed(2);
    }

    const sceneIdx = t < 2.8 || t >= 9.3 ? 0 : t < 5.25 ? 1 : 2;
    beatPills.forEach((pill, idx) => {
      pill.dataset.active = String(idx === sceneIdx);
    });

    if (phaseBadgeEl) {
      if (sceneIdx === 0) {
        phaseBadgeEl.dataset.phase = "flood";
        phaseBadgeEl.textContent = compact ? "SCENE 01 · GREP FLOOD" : "SCENE 01 · 200L GREP FLOOD";
      } else if (sceneIdx === 1) {
        phaseBadgeEl.dataset.phase = "clamp";
        phaseBadgeEl.textContent = compact ? "SCENE 02 · OPTICAL WIPE" : "SCENE 02 · TRIGRAM OPTICAL WIPE";
      } else {
        phaseBadgeEl.dataset.phase = "hit";
        phaseBadgeEl.textContent = compact ? "SCENE 03 · 0.23 ms HIT" : "SCENE 03 · AST CLAMP LOCKED";
      }
    }

    let wipeU = 0;
    if (t >= 2.8 && t < 3.95) {
      wipeU = lerp(0, 0.62, minJerk((t - 2.8) / 1.15));
    } else if (t >= 3.95 && t < 4.35) {
      wipeU = lerp(0.62, 0.65, (t - 3.95) / 0.4);
    } else if (t >= 4.35 && t < 5.25) {
      wipeU = lerp(0.65, 1.0, minJerk((t - 4.35) / 0.9));
    } else if (t >= 5.25) {
      wipeU = 1.0;
    }

    const dissolveP = t >= 8.8 ? minJerk((t - 8.8) / 1.2) : 0;

    if (wipeU < 1.0) {
      paintStateA(ctxA, t, m);
    } else if (dissolveP > 0) {
      paintStateA(ctxA, 0.0, m);
    }

    if (wipeU > 0.0) {
      paintStateB(ctxB, t, m);
    }

    ctx!.save();
    ctx!.clearRect(0, 0, canvas!.width, canvas!.height);

    if (wipeU <= 0.001) {
      ctx!.drawImage(bufA, 0, 0);
    } else if (wipeU >= 0.999 && dissolveP <= 0.001) {
      ctx!.drawImage(bufB, 0, 0);
    } else if (dissolveP > 0.001) {
      ctx!.drawImage(bufB, 0, 0);
      ctx!.globalAlpha = dissolveP;
      ctx!.drawImage(bufA, 0, 0);
      ctx!.globalAlpha = 1;
    } else {
      ctx!.drawImage(bufA, 0, 0);

      ctx!.save();
      ctx!.scale(dpr, dpr);

      const tilt = 22;
      const bladeX = lerp(-36, w + 36, wipeU);

      // Soft cast shadow onto State A to the right of the blade
      const shGrad = ctx!.createLinearGradient(bladeX, 0, bladeX + 42, 0);
      shGrad.addColorStop(0, "rgba(3, 8, 20, 0.62)");
      shGrad.addColorStop(1, "rgba(3, 8, 20, 0)");
      ctx!.fillStyle = shGrad;
      ctx!.beginPath();
      ctx!.moveTo(bladeX + tilt, 0);
      ctx!.lineTo(bladeX + tilt + 46, 0);
      ctx!.lineTo(bladeX - tilt + 46, h);
      ctx!.lineTo(bladeX - tilt, h);
      ctx!.closePath();
      ctx!.fill();

      // Clip left region to draw State B cleanly
      ctx!.save();
      ctx!.beginPath();
      ctx!.moveTo(-50, 0);
      ctx!.lineTo(bladeX + tilt, 0);
      ctx!.lineTo(bladeX - tilt, h);
      ctx!.lineTo(-50, h);
      ctx!.closePath();
      ctx!.clip();
      ctx!.setTransform(1, 0, 0, 1, 0, 0);
      ctx!.drawImage(bufB, 0, 0);
      ctx!.restore();

      // Dual-glow optical wipe blade
      ctx!.strokeStyle = "rgba(84, 234, 210, 0.26)";
      ctx!.lineWidth = 14;
      ctx!.beginPath();
      ctx!.moveTo(bladeX + tilt, 0);
      ctx!.lineTo(bladeX - tilt, h);
      ctx!.stroke();

      ctx!.strokeStyle = "#54ead2";
      ctx!.lineWidth = 2.4;
      ctx!.beginPath();
      ctx!.moveTo(bladeX + tilt, 0);
      ctx!.lineTo(bladeX - tilt, h);
      ctx!.stroke();

      // Tactile circular lens handle at vertical midpoint
      const hy = h * 0.5;
      ctx!.beginPath();
      ctx!.arc(bladeX, hy, 13, 0, Math.PI * 2);
      ctx!.fillStyle = "#0b192e";
      ctx!.fill();
      ctx!.strokeStyle = "#54ead2";
      ctx!.lineWidth = 2;
      ctx!.stroke();

      ctx!.font = `700 10px ${MONO_FONT}`;
      ctx!.fillStyle = "#54ead2";
      ctx!.textAlign = "center";
      ctx!.textBaseline = "middle";
      ctx!.fillText("‹›", bladeX, hy + 0.5);

      if (!compact && bladeX > 105 && bladeX < w - 105) {
        const chipY = hy - 34;
        const drawChip = (cx: number, label: string, bg: string, border: string, fg: string) => {
          drawRoundRect(ctx!, cx - 42, chipY, 84, 18, 99);
          ctx!.fillStyle = bg;
          ctx!.fill();
          ctx!.strokeStyle = border;
          ctx!.lineWidth = 1;
          ctx!.stroke();
          ctx!.font = `700 8.6px ${MONO_FONT}`;
          ctx!.fillStyle = fg;
          ctx!.fillText(label, cx, chipY + 9.5);
        };
        drawChip(bladeX - 54, "WITH CCTX", "rgba(9, 28, 44, 0.94)", "rgba(84, 234, 210, 0.6)", "#54ead2");
        drawChip(bladeX + 54, "RAW GREP", "rgba(38, 15, 28, 0.94)", "rgba(255, 135, 135, 0.55)", "#ffa8a8");
      }

      ctx!.restore();
    }

    ctx!.restore();
  }

  let heroVisible = true;
  const io = new IntersectionObserver(
    (entries) => {
      heroVisible = entries.some((e) => e.isIntersecting);
      if (heroVisible) lastWallMs = performance.now();
    },
    { threshold: 0.05 },
  );
  io.observe(cinema);

  resize();
  new ResizeObserver(resize).observe(screen);

  const tick = (nowMs: number) => {
    const dt = Math.min(0.08, Math.max(0, (nowMs - lastWallMs) / 1000));
    lastWallMs = nowMs;

    if (seekToT !== null) {
      seekElapsed += dt;
      const p = clamp(seekElapsed / SEEK_DURATION);
      currentT = lerp(seekFromT, seekToT, minJerk(p));
      if (p >= 1) {
        currentT = seekToT;
        seekToT = null;
      }
      renderFrame(currentT);
    } else if (playing && heroVisible) {
      currentT = (currentT + dt) % HERO_DURATION;
      renderFrame(currentT);
    }

    requestAnimationFrame(tick);
  };

  renderFrame(currentT);
  requestAnimationFrame(tick);
}

function easeOutCubic(p: number): number {
  const x = clamp(p);
  return 1 - Math.pow(1 - x, 3);
}

function easeInOutCubic(p: number): number {
  const x = clamp(p);
  return x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2;
}

function drawTelemetryRow(
  g: CanvasRenderingContext2D,
  s: { pad: number; codeW: number; gaugeY: number; gaugeH: number },
  sm: StageMetrics,
  opts: {
    accent: string;
    border: string;
    bg: string;
    items: Array<{ label: string; val: string; sub: string }>;
    glintP: number;
  },
): void {
  const gap = sm.compact ? 6 : 9;
  const cardW = (s.codeW - gap * 2) / 3;

  opts.items.forEach((it, idx) => {
    const x = s.pad + idx * (cardW + gap);
    const y = s.gaugeY;

    drawRoundRect(g, x, y, cardW, s.gaugeH, 8);
    g.fillStyle = opts.bg;
    g.fill();
    g.strokeStyle = opts.border;
    g.lineWidth = 1;
    g.stroke();

    if (opts.glintP >= 0 && opts.glintP <= 1) {
      g.save();
      drawRoundRect(g, x, y, cardW, s.gaugeH, 8);
      g.clip();
      const gx = lerp(s.pad - 60, s.pad + s.codeW + 60, easeInOutCubic(opts.glintP));
      const gl = g.createLinearGradient(gx - 36, y, gx + 36, y + 20);
      gl.addColorStop(0, "rgba(255,255,255,0)");
      gl.addColorStop(0.5, "rgba(84, 234, 210, 0.22)");
      gl.addColorStop(1, "rgba(255,255,255,0)");
      g.fillStyle = gl;
      g.fillRect(x, y, cardW, s.gaugeH);
      g.restore();
    }

    g.textBaseline = "middle";
    g.font = `700 ${sm.compact ? 7.8 : 8.8}px ${MONO_FONT}`;
    g.fillStyle = "#8aa0c4";
    g.fillText(it.label, x + 8, y + 13);

    g.font = `800 ${sm.compact ? 12.2 : 14.5}px ${MONO_FONT}`;
    g.fillStyle = "#ffffff";
    g.fillText(it.val, x + 8, y + (sm.compact ? 29 : 32));

    g.font = `600 ${sm.compact ? 8.2 : 9.4}px ${SANS_FONT}`;
    g.fillStyle = opts.accent;
    g.fillText(it.sub, x + 8, y + s.gaugeH - 11);
  });
}

/* ==========================================================================
   2. ACT 2 — Shared Rust Daemon Pipeline Stage (Edge-to-Edge Comet Pulses)
   ========================================================================== */
function initEnginePipelineStage(reducedMotion: boolean): void {
  const section = document.getElementById("cctx-engine-section");
  const canvas = document.getElementById("cctx-engine-canvas") as HTMLCanvasElement | null;
  if (!section || !canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const nodeCards = document.querySelectorAll<HTMLElement>("[data-engine-step]");
  const cacheModeBtns = document.querySelectorAll<HTMLButtonElement>("[data-cache-mode]");

  let cacheMode: "warm" | "cold" = "warm";
  let pinnedStep: number | null = null;
  let activeScrollIdx = 2;
  let clock = 0;
  let lastMs = performance.now();
  let visible = false;
  let w = 1;
  let h = 1;
  let dpr = 1;

  const updateScrollStep = () => {
    const sRect = section.getBoundingClientRect();
    const prog = clamp((window.innerHeight * 0.72 - sRect.top) / Math.max(240, sRect.height));
    activeScrollIdx = Math.min(3, Math.floor(prog * 4));
    const shown = pinnedStep !== null ? pinnedStep : activeScrollIdx;
    nodeCards.forEach((c, i) => {
      c.dataset.active = String(i === shown);
    });
  };

  const resize = () => {
    const rect = canvas.getBoundingClientRect();
    w = Math.max(1, rect.width);
    h = Math.max(1, rect.height);
    dpr = Math.min(2, window.devicePixelRatio || 1);
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
    updateScrollStep();
    drawPipeline(clock);
  };

  cacheModeBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      cacheMode = (btn.dataset.cacheMode as "warm" | "cold") || "warm";
      cacheModeBtns.forEach((b) => b.setAttribute("aria-pressed", String(b === btn)));
      drawPipeline(clock);
    });
  });

  nodeCards.forEach((card, idx) => {
    card.addEventListener("click", () => {
      pinnedStep = pinnedStep === idx ? null : idx;
      updateScrollStep();
      drawPipeline(clock);
    });
  });

  function drawCometSegment(
    ax: number,
    ay: number,
    bx: number,
    by: number,
    u: number,
    color: string,
  ): void {
    for (let k = 0; k < 6; k++) {
      const uk = u - k * 0.045;
      if (uk < 0 || uk > 1) continue;
      const px = lerp(ax, bx, uk);
      const py = lerp(ay, by, uk);
      const r = lerp(4.2, 1.4, k / 5);
      ctx!.beginPath();
      ctx!.arc(px, py, r, 0, Math.PI * 2);
      ctx!.fillStyle = color;
      ctx!.globalAlpha = lerp(0.95, 0.12, k / 5);
      ctx!.fill();
    }
    ctx!.globalAlpha = 1;
  }

  function drawPipeline(t: number): void {
    ctx!.save();
    ctx!.scale(dpr, dpr);
    ctx!.clearRect(0, 0, w, h);

    const isMobile = w < 580;
    const activeIdx = pinnedStep !== null ? pinnedStep : activeScrollIdx;
    const accent = cacheMode === "warm" ? "#54ead2" : "#ffd43b";

    const labels =
      cacheMode === "warm"
        ? [
            { title: "1. Agent Call", sub: "#cctxNative* / MCP" },
            { title: "2. Trigram + HIR", sub: "Case-folded postings" },
            { title: "3. AST Block Clamp", sub: "<8 KiB + SHA-256" },
            { title: "4. 0.232 ms Return", sub: "Native + MCP fan-out" },
          ]
        : [
            { title: "1. Cold Root Call", sub: "Unindexed workspace" },
            { title: "2. Instant CACHE_MISS", sub: "0 ms stall · host cont." },
            { title: "3. Async Indexer", sub: "19.7 ms background" },
            { title: "4. Next Turn Warm", sub: "98.6% hit rate" },
          ];

    const boxW = isMobile ? Math.min(256, w - 36) : Math.min(162, (w - 84) / 4.35);
    const boxH = isMobile ? 50 : 68;

    const points = labels.map((_, i) => {
      const frac = i / (labels.length - 1);
      return isMobile
        ? { x: w * 0.5, y: 36 + frac * (h - 72) }
        : { x: 24 + boxW * 0.5 + frac * (w - 48 - boxW), y: h * 0.5 };
    });

    for (let i = 0; i < points.length - 1; i++) {
      const p0 = points[i];
      const p1 = points[i + 1];
      const ax = isMobile ? p0.x : p0.x + boxW * 0.5;
      const ay = isMobile ? p0.y + boxH * 0.5 : p0.y;
      const bx = isMobile ? p1.x : p1.x - boxW * 0.5;
      const by = isMobile ? p1.y - boxH * 0.5 : p1.y;

      ctx!.strokeStyle = "rgba(62, 90, 142, 0.45)";
      ctx!.lineWidth = 2.5;
      ctx!.setLineDash(cacheMode === "cold" && i === 1 ? [5, 4] : []);
      ctx!.beginPath();
      ctx!.moveTo(ax, ay);
      ctx!.lineTo(bx, by);
      ctx!.stroke();
      ctx!.setLineDash([]);

      if (i < activeIdx + 1) {
        ctx!.strokeStyle =
          cacheMode === "cold" && i >= 1 ? "rgba(116, 143, 252, 0.7)" : "rgba(84, 234, 210, 0.65)";
        ctx!.lineWidth = 2.2;
        ctx!.beginPath();
        ctx!.moveTo(ax, ay);
        ctx!.lineTo(bx, by);
        ctx!.stroke();
      }

      const segU = (((t * 0.85 - i * 0.32) % 1.35) + 1.35) % 1.35;
      if (segU <= 1.0) {
        drawCometSegment(
          ax,
          ay,
          bx,
          by,
          minJerk(segU),
          cacheMode === "cold" && i >= 1 ? "#91a7ff" : accent,
        );
      }
    }

    ctx!.textAlign = "center";
    ctx!.textBaseline = "middle";
    points.forEach((pt, i) => {
      const isSelected = i === activeIdx;
      const bx = pt.x - boxW * 0.5;
      const by = pt.y - boxH * 0.5;

      const arrivalPhase = (((t * 0.85 - (i - 1) * 0.32 - 0.95) % 1.35) + 1.35) % 1.35;
      if (i > 0 && arrivalPhase < 0.35) {
        const hp = arrivalPhase / 0.35;
        drawRoundRect(ctx!, bx - hp * 6, by - hp * 5, boxW + hp * 12, boxH + hp * 10, 12);
        ctx!.strokeStyle = `rgba(84, 234, 210, ${((1 - hp) * 0.45).toFixed(3)})`;
        ctx!.lineWidth = 1.5;
        ctx!.stroke();
      }

      drawRoundRect(ctx!, bx, by, boxW, boxH, 10);
      ctx!.fillStyle = isSelected ? "#16294d" : "#0e182d";
      ctx!.fill();
      ctx!.strokeStyle = isSelected ? accent : "#2a416b";
      ctx!.lineWidth = isSelected ? 2 : 1.2;
      ctx!.stroke();

      ctx!.fillStyle = "#ffffff";
      ctx!.font = `700 12px ${SANS_FONT}`;
      ctx!.fillText(labels[i].title, pt.x, pt.y - 8);

      ctx!.fillStyle = isSelected ? accent : "#9bb0d3";
      ctx!.font = `500 10.2px ${MONO_FONT}`;
      ctx!.fillText(labels[i].sub, pt.x, pt.y + 11);
    });

    ctx!.restore();
  }

  const io = new IntersectionObserver(
    (entries) => {
      visible = entries.some((e) => e.isIntersecting);
      if (visible) {
        lastMs = performance.now();
        updateScrollStep();
      }
    },
    { threshold: 0.05 },
  );
  io.observe(section);

  window.addEventListener(
    "scroll",
    () => {
      updateScrollStep();
      if (reducedMotion) drawPipeline(clock);
    },
    { passive: true },
  );

  resize();
  new ResizeObserver(resize).observe(canvas);

  if (reducedMotion) {
    drawPipeline(0.45);
    return;
  }

  const loop = (now: number) => {
    const dt = Math.min(0.08, Math.max(0, (now - lastMs) / 1000));
    lastMs = now;
    if (visible) {
      clock += dt;
      drawPipeline(clock);
    }
    requestAnimationFrame(loop);
  };

  drawPipeline(0);
  requestAnimationFrame(loop);
}

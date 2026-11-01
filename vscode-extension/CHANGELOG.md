# Changelog

## 0.21.0

- Boost exact whole-word identifier matches (`has_whole_word_match`) above
  substring definition matches under `max_matches_per_file` and demote
  `@generated` / `DO NOT EDIT` files so symbol lookups resolve directly to
  human-authored definitions.
- Fix block clamping for the first, adjacent, and last methods inside `impl`,
  `trait`, `class`, `interface`, and `mod` blocks where no blank line separates
  the method from `{` or `}`, and fix index-0 definition header indentation
  tracking.
- Reclaim heap memory in `PostingList::remove_sorted` when lists shrink to
  `<= 5` entries, surface `[Cell Visual Output: image/png]` markers in Jupyter
  notebooks, bound 3+ range default read spans to prevent `>8 KiB` host
  offloading, and report estimated lookup tokens saved in the VS Code status bar
  tooltip.

## 0.20.0

- Expand `PostingList::Inline` capacity to 5 document IDs without increasing
  bucket size (`24 B`), lower multi-threaded trigram indexing threshold to
  `512 KiB`, add CJK UTF-8 syllable boundary selectivity scoring and 16-window
  long-literal sampling, bypass trigram lookups when `<=2` files are scoped,
  and partition parallel regex workers by byte size.
- Add 2-class small character set planning (`[gs]et_user_[iI]d`) and redundant
  subslice literal elimination in the HIR planner, negated glob filters
  (`*.rs, !*.test.rs`), case-insensitive extension matching across all source
  files, and multi-token subsequence filename fallback in `code_files`.
- Add Git merge conflict (`<<<<<<<` .. `>>>>>>>`), TOML section (`[table]`),
  and YAML OpenAPI/workflow block clamping, cap leading doc-comment rewind to
  12 lines, extend multi-line `impl`/`trait`/`class`/`interface` headers, and
  recognize CSS custom properties (`--var:`), `#[proc_macro*]`, Mermaid diagrams,
  and PlantUML blocks.
- Compact `#cctxNativeStatus` and `#cctxNativeWorkspaces` JSON payloads by >93%
  so responses stay inline (`<3 KB`) instead of being offloaded to `/tmp`,
  support `.ipynb#cell-N` / `#slide-N` / `#page-N` / `#record-N` anchors,
  GitHub/GitLab blob URLs, and `rg` `path:line: text` inputs, and preserve
  `.tsbuildinfo` and `.eslintcache` across extension builds.

## 0.19.0

- Accelerate coding, build, and test cycles by configuring `line-tables-only`
  debug symbols and `opt-level = 2` dependency compilation in `dev`/`test`
  profiles, cutting `cargo test` wall-clock runtime by >65%.
- Reduce daemon active background polling latency (`25 ms → 5 ms`) and watcher
  atomic-save read recovery delay (`25 ms → 8 ms`), and skip post-limit line
  galloping with a `memchr` newline jump fast path in resident and query scans.
- Extend signature-complete clamping to Rust `where` clauses, Python class
  methods immediately following `class` headers, C++ `template<...>` prefixes,
  C# `[Attribute]` annotations, and Go receiver method definitions (`func (s
  *Server)`), and enable incremental TypeScript builds (`incremental: true`).

## 0.18.0

- Prevent VS Code agent edit retries (`No match found` / `Multiple matches found`)
  and compile-fix rewrite loops by preserving multi-line function signatures,
  `where` clauses, return types, multi-line `#[derive(...)]` / `/** ... */`
  attributes, full `struct` / `interface` / `enum` / `type` field blocks, and
  TypeScript/Rust overload + implementation pairs in `code_context`.
- Track outer definition indentation during block clamping so inner `if`/`match`
  closing braces (`}`) never truncate enclosing functions, and recognize chained
  error-propagation closures (`})?`, `})?;`, `}).`).
- Guarantee surrounding context lines for short/non-unique call-site hits, add
  test-attribute (`#[test]`, `#[tokio::test]`, `#[rstest]`) definition boosting,
  and demote debug print macros (`println!`, `eprintln!`, `dbg!`) under
  `max_matches_per_file`.
- Eliminate tool-invocation retries by auto-normalizing `**.ext` and `.ext`
  shorthand globs, splitting comma-separated `paths`, converting Windows
  backslash relative paths, handling `start=0` / reversed `start > end` /
  `view_range: [start, -1]` in MCP and Native reads, reporting `valid_tail`
  bounds on `READ_RANGE_OUT_OF_BOUNDS`, and surfacing a `regex=true` hint when
  a literal search with regex syntax returns zero matches.

## 0.17.0

- Expand polyglot symbol-definition ranking and smart block-clamped context
  assembly across Go, Kotlin, Scala, Swift, Elixir, Ruby, PHP, Lua, Dart, SQL
  DDL, Protobuf/gRPC, GraphQL, Terraform/HCL/Bicep, Makefile/Justfile recipes,
  Dockerfile multi-stage builds, Python 3.12+ `type` aliases and multi-line
  decorators, React 19 / TanStack / Next.js / Svelte 5 Runes / Vue 3.5+ /
  Angular Signals, and comment-only hit demotion with post-clamping snippet
  coalescing.
- Accelerate UTF-8 / CJK / Korean + ASCII `ignore_case` regex queries and
  grouped small character classes (`\b(?:prefix)[-_]suffix\b`) in the trigram
  query planner, and add cross-convention (`camelCase` / `snake_case` /
  `kebab-case`) filename stem fallback in `code_files`.
- Add JSON Lines (`.jsonl`, `.ndjson`, `.jsonlines`) record extraction and
  structure inspection, XLIFF 2.0 `<unit>`/`<note>` support, WebVTT/SRT cue
  extraction and clamping, Jupyter `In[N]` execution counts and ANSI-cleaned
  error tracebacks, SVG structure inspection, inline code/HTML stripping in
  Markdown `#slug` anchors, and 2026 polyglot source/exclusion profiles.
- Support `around_line` / `center_line` + `radius` centered reads, Python
  traceback (`File "path", line N`) and Rust/Go compiler diagnostic (`-->
  path:line:col`) target parsing, `#subpath` package aliases, `cursor://` /
  `windsurf://` / `vscode-insiders://` URIs, MSYS paths, and consecutive line-run
  compression (`20-22`) in search output.

## 0.16.0

- Add native zero-dependency document indexing and smart section/slide/sheet/table
  clamping for Office (`docx`, `pptx`, `xlsx`), OpenDocument (`odt`, `odp`, `ods`),
  `epub`, `pdf`, `rtf`, Jupyter `ipynb`, `csv`/`tsv`, LaTeX/BibTeX, Typst, AsciiDoc,
  Org, XLIFF/gettext, and WebVTT/SRT.
- Enhance Markdown (`.md`, `.mdx`, `.markdown`, `.qmd`, `.rmd`, and agent
  instructions) with fence-aware ATX/Setext heading detection, GFM alerts,
  `:::` admonitions, `<details>`, pipeless GFM tables, `$$` math blocks, and
  `path.md#heading-slug` section range reads.
- Improve React, Vite, TypeScript/TSX, Vue/Svelte/Astro SFC, and CSS/Tailwind
  definition ranking, barrel re-export filtering, dynamic-route globs
  (`[id].tsx`), compound extension fast paths (`*.test.tsx`, `*.module.css`),
  Vite `?raw`/`?react` query stripping, `@/` alias resolution, and lockfile/bundle
  exclusions.
- Expand VS Code GitHub Copilot compatibility with built-in tool parameter
  aliases (`includePattern`, `symbol`, `isRegexp`, `caseInsensitive`,
  `maxResults`, `contextLines`, `filePath`, `view_range`), Markdown link and
  backtick path unwrapping, and automatic `>500`-line read chunking.

## 0.15.0

- Eliminate multi-project VS Code startup contention with registry seeding,
  coalesced `code_workspaces` discovery, jittered service auto-start, lock-free
  registered-root preparation, and small-root background queue prioritization.
- Prioritize native VS Code Language Model Tools (`#cctxNative*`) first and add
  smart multi-root workspace resolution across visible editors, tab groups,
  relative path existence checks, folder basenames, file URIs, and `:line` /
  `#Lline` suffixes.
- Accelerate resident trigram lookups (`15.01×` mixed warm speedup, `0.260 ms`
  mean across 8,960 pairs; `20.50×` literal search) with inline `PostingList`
  storage, SplitMix64 trigram maps, galloping line search, zero-copy `CCTXJ1` /
  `CCTXB1` IPC framing, and adaptive definition vs. call-site snippet clamping.
- Update the four-panel benchmark chart to compare `CCTX off`, `v0.14`, and
  current warm IPC side-by-side with explicit latency, speedup, and token labels.

## 0.14.0

- Support native Windows (`cctx.exe`) workspaces without WSL alongside existing
  Linux, WSL, and SSH hosts, with hidden background daemon auto-start and
  drive-letter path normalization.
- Attach contributed chat instructions automatically (`applyTo: "**"`) and
  guide deferred `tool_search` discovery so new installations use Code Context
  out of the box.
- Accelerate `ignore_case` and scoped searches via ASCII case-folded trigram
  planning and selectivity-first filtering; recover transparently from stale
  index snapshots on search/context calls.
- Add `max_matches_per_file` with symbol-definition priority, smart block/Python
  boundary snippet clamping, and compact trace/notice output to cut payload and
  schema tokens.
- Publish the four-panel retrieval speedup, Rust/Python token savings, and
  end-to-end coding agent acceleration benchmark figure.

## 0.13.2

- Render native Output traces, status, self-test, and registration as readable
  text; use real log levels and preserve machine-facing JSON contracts.
- Emit one call summary with timings and correlation; record root-resolution
  failures and escape control characters in error text.
- Treat Windows Agents' explicit-root requirement as callable readiness instead
  of an unregistered error, and skip unrelated session-folder readiness probes.
- Avoid registry enumeration for explicit WSL roots while retaining backend
  authorization; let Windows status choose a registered Linux root.

## 0.13.1

- Qualify the Marketplace display name as **Code Context by dotnetpower** after
  the unqualified display name was rejected as already taken.
- Preserve the publisher, extension ID, settings, and seven native references.
- Verify that both the packaged manifest and VSIX metadata use the corrected
  display name.

## 0.13.0

- Adopt `dotnetpower.code-context-native` for the first Marketplace publication.
- Resolve walkthroughs, settings, and self-test metadata from the running
  extension identity instead of a previous publisher.
- Keep all seven native tool references, settings, and the shared backend stable.
- Refuse duplicate registration while the previous publisher's extension is
  present; document profile/host migration and safe-boundary reloads.
- Validate both VSIX and package identities and allow the public publisher brand
  in docs while continuing to reject machine-specific paths.

## 0.12.3

- Put Resources links and the complete benchmark table before installation in
  extension details, retaining results even when images cannot load.
- Explain the Marketplace-only native Resources sidebar and manual VSIX updates.
- Validate the packaged README, image, and gallery-ready resource properties.

## 0.12.2

- Simplify installation READMEs and move detailed operation notes to dedicated guides.
- Replace local machine/project examples with portable placeholders.
- Publish a correctness-checked AKS Store Demo on/off retrieval benchmark, with
  session-bootstrap confidence intervals, cold behavior, and slower known reads.
- Include the benchmark figure in the VSIX; preserve the existing extension identity.

## 0.12.1

- Supply required, packaged walkthrough media for all four setup steps.
- Verify those media entries against the built VSIX as well as the manifest.
- Keep the first installable GitHub distribution on this corrected patch version.

## 0.12.0

### Added
- Original icon and gallery artwork, a concise extension landing page, and
  dedicated setup and publishing guides.
- A four-step Getting Started walkthrough and connection/help commands.
- Reproducible VSIX validation and GitHub release packaging.

### Changed
- Marketplace-facing display name is **Code Context**; the extension ID and all
  seven native tool IDs/references remain unchanged.
- Stable tool contributions replace the proposed tool-set API. Installation no
  longer edits VS Code runtime arguments or needs proposed API approval.
- Tool grouping is now managed by VS Code. Review tool enablement after upgrade;
  per-tool IDs/references remain unchanged.
- Normal Linux/WSL/SSH and Windows Agent Sessions connection paths are preserved.

## 0.11.1

- Negotiated outcome and Windows memory trace fields for older daemon compatibility.

## 0.11.0

- Event-driven native usage status and separate success/readiness/refusal/error outcomes.
- Adaptive host/cgroup-aware backend memory, including Windows physical headroom.
- More precise source refusal diagnostics and directory-only build-output exclusions.

## 0.10.5

- Windows Agents activation and native WSL IPC support.
- Explicit trust enforcement without relying on a mismatched UI context key.

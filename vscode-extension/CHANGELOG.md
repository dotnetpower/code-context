# Changelog

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

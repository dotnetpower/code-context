# Changelog

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

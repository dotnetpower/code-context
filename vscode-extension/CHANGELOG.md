# Changelog

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

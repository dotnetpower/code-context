# One engine. Every chat.

```text
VS Code native tools
        |
        v
Shared cctx daemon
        |
        v
Saved source + local cache
```

Install the backend inside Linux, WSL, or your Linux SSH host.

```sh
git clone https://github.com/dotnetpower/code-context.git
cd code-context
bash scripts/install.sh
~/.local/bin/cctx ping
```

**Required:** current Rust, ripgrep, and a Linux user service.
The extension does not download executables without your action.

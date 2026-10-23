# Connect where the engine lives

## Linux workspace

Leave the socket path empty to use the default local runtime.
WSL and SSH workspace windows use the Linux workspace host too.

## Windows Agent Sessions

Configure the actual distribution and Linux executable:

```json
{
  "codeContextNative.wslDistribution": "Ubuntu-22.04",
  "codeContextNative.wslExecutablePath": "/home/you/.local/bin/cctx"
}
```

The setup guide includes the Agents-workspace UI-host setting.
This path uses native IPC through WSL, not a second index or a TCP server.

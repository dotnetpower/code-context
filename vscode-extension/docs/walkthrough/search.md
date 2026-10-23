# Ask for the right context

**Find a filename**

```text
#cctxNativeFiles
glob: *Service.ts
```

**Find a symbol and its nearby source**

```text
#cctxNativeContext
pattern: createService
```

Use your actual registered Linux root in Windows Agents.
Patterns are literal unless `regex: true` is explicit.

A cold cache returns `CACHE_MISS` immediately. Continue with permitted normal
tools instead of polling or syncing to wait for warmup.

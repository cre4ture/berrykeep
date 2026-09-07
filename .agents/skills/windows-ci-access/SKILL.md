---
name: windows-ci-access
description: Reproduce or debug BerryKeep CI failures that require native Windows behavior. Use from a non-Windows host for CFAPI, Cloud Files, or other Windows-only failures that need the Windows CI environment.
---

# Windows CI Access

Prefer environment parity: use native Windows rather than WSL for CFAPI, Cloud Files, placeholder hydration, and other Windows-only filesystem behavior.

## Access

- SSH: `ssh Uli@192.168.178.129 -p 2222`
- Repository: `C:\Users\Uli\rust-dev\ironmesh`

## Workflow

- Start with the minimal reproducer: run the smallest failing Windows test or command first.
- For one-off remote commands, prefer `pwsh -NoProfile -Command ...`.
- Work from `C:\Users\Uli\rust-dev\ironmesh` and keep Windows-specific investigation native to Windows.

Example:

```text
ssh Uli@192.168.178.129 -p 2222 "pwsh -NoProfile -Command \"Set-Location 'C:\Users\Uli\rust-dev\ironmesh'; cargo test --manifest-path tests/system-tests/Cargo.toml\""
```

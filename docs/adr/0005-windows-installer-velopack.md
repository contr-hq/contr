# 0005. Windows ships through Velopack

## Context

The Windows client bundles the core release and needs a per-user installer, an MSI for Intune and Group Policy, and
delta updates. WiX v7 requires the Open Source Maintenance Fee above a revenue threshold, and Inno Setup asks commercial
users to buy a license.

## Decision

Package `contr-windows` with Velopack (MIT): per-user `Setup.exe`, MSI for managed deployment, delta updates from GitHub
Releases, Windows App SDK self-contained. Binaries are Authenticode-signed.

## Consequences

- No elevation for normal installs and updates.
- Velopack's claim that its bundled WiX fork is outside the maintenance fee needs legal confirmation before 1.0.

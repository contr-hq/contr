# 0008. Windows packages carry every runtime

## Context

Users must not install anything before or after Contr. On a clean Windows machine the client needs the .NET runtime,
the Windows App SDK runtime and WebView2; the Rust binaries need the Visual C++ runtime; the engine needs the WSL
guest images and WSL itself. PowerToys ships a per-user installer by default and a machine-wide MSI, both carrying
.NET and the Windows App SDK; it builds them with WiX v5 and its own bootstrapper. WiX v6 and later require the Open
Source Maintenance Fee for most companies. Velopack (ADR 0005) builds the same two formats with its own WiX fork and
adds delta updates.

## Decision

- Each release ships, per architecture, `Contr-<version>-<arch>-Setup.exe` (per-user under `%LOCALAPPDATA%`, no
  elevation, the default) and `Contr-<version>-<arch>.msi` (per-machine under `Program Files`, for Intune, Group
  Policy and `msiexec /qn`), both built by Velopack from one self-contained publish.
- Runtimes are app-local: the client is published self-contained with `WindowsAppSDKSelfContained`, and the Rust
  binaries link the C runtime statically (`+crt-static`), so neither the .NET runtime, the Windows App SDK runtime
  nor the Visual C++ Redistributable is installed.
- The core release publishes `contr-core-windows-x64-<version>.zip`: `contrd.exe`, `contr.exe`, `guest/` and `cli/`
  in the layout `contrd` expects beside itself, with `core.json` pinning every file by SHA-256. The client pins one
  payload by version and digest and lays it out under its install directory.
- WebView2 is the Evergreen runtime, which Windows 11 includes; `Setup.exe` installs it when it is missing.
- WSL is a Windows component: onboarding installs it through the `install-wsl` remediation, with the one elevation
  prompt Contr shows.

## Consequences

- Neither installer asks for anything else on Windows 11; an offline Windows 10 machine without WebView2 needs it
  deployed with the MSI.
- Packages grow by the .NET and Windows App SDK runtimes (about 100 MB) and the guest images.
- The release workflow fails when a Windows binary imports the Visual C++ runtime.
- Velopack's position that its WiX fork is outside the maintenance fee still needs legal confirmation before 1.0.
- Shipping the WSL MSI inside the payload for offline installs is open until its redistribution terms are confirmed.

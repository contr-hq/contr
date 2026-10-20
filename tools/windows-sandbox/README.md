# Windows 11 sandbox

A Windows 11 VM on the Linux development machine. It builds and runs the Windows side of Contr: `contrd` for
Windows, the WSL backend (nested WSL 2), and the WinUI 3 client. `sandbox` creates it unattended, and every later
interaction (commands, file sync, GUI launch, screenshots) goes through SSH or libvirt, so people and agents use it
the same way.

## Requirements

- x86-64 Linux with KVM and **nested virtualization** enabled (`/sys/module/kvm_{amd,intel}/parameters/nested`)
- libvirt with the `default` NAT network, your user in the `libvirt` group
- `swtpm`, OVMF with Microsoft Secure Boot keys (`ovmf` package), `xorriso`, `jq`, `python3`
- Disk: 9 GB for the ISOs in the libvirt pool plus the VM disk (128 GB sparse, about 50 GB used after provisioning)
- Media in `~/.cache/contr-sandbox/downloads/` (override with `SANDBOX_WINDOWS_ISO` / `SANDBOX_VIRTIO_ISO`):
  - Windows 11 Enterprise Evaluation ISO from the
    [Microsoft Evaluation Center](https://www.microsoft.com/evalcenter/download-windows-11-enterprise)
  - [virtio-win](https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/) driver ISO

The evaluation edition runs for 90 days. Rebuilding is `sandbox destroy --yes && sandbox create && sandbox wait`; the
ISOs stay in the pool.

## Use

```sh
tools/windows-sandbox/sandbox create     # about 5 minutes: uploads media, defines the VM, starts Windows Setup
tools/windows-sandbox/sandbox wait       # about 60-90 minutes: Windows install, then tool provisioning
tools/windows-sandbox/sandbox status
```

| Command | Effect |
|---|---|
| `ssh [command]` | PowerShell 7 session as `dev`, or one command |
| `sync <dir> [guest-dir]` | Copies a git working tree's tracked and untracked-unignored files to `C:/src/<name>` and deletes files removed on the host; `bin`, `obj`, `target` and similar build directories are left alone |
| `gui <program> [args]` | Starts a program on the logged-on desktop (SSH sessions have no desktop) |
| `screenshot [file]` | Saves the display as PNG |
| `console` | Opens the display in virt-manager |
| `rdp` | Prints Remote Desktop details; the password is in `~/.local/share/contr-sandbox/password` |
| `start`, `stop`, `destroy [--purge] [--yes]` | Lifecycle |

A typical loop, edited on Linux and built and run on Windows:

```sh
tools/windows-sandbox/sandbox sync .
tools/windows-sandbox/sandbox ssh 'cd C:/src/dockercraft; cargo build --workspace'
tools/windows-sandbox/sandbox sync ../contr-windows
tools/windows-sandbox/sandbox ssh 'cd C:/src/contr-windows; dotnet build -p:Platform=x64'
tools/windows-sandbox/sandbox gui 'C:\src\contr-windows\...\Contr.exe'
tools/windows-sandbox/sandbox screenshot
```

Before SSH is up, `wait` and `status` read provisioning progress through the QEMU guest agent.

## What gets installed

Provisioning runs as SYSTEM from a scheduled task, logs to `C:\contr-sandbox\provision.log`, and records progress in
`C:\contr-sandbox\status.json`. Each step leaves a marker in `C:\contr-sandbox\state`, so a failed step is retried
from the next boot without repeating finished ones.

| Component | Source |
|---|---|
| virtio drivers, QEMU guest agent, SPICE agent | virtio-win guest tools |
| OpenSSH server, key-only login for `dev`, default shell PowerShell 7 | Windows capability |
| PowerShell 7, Git, GitHub CLI, WSL | Latest GitHub releases |
| .NET 10 SDK | `dotnet-install.ps1 -Channel 10.0` |
| Visual Studio 2026 Build Tools, C++ workload with Windows SDK | `aka.ms/vs/18/stable` |
| Rust (stable, MSVC) shared by all users | rustup, `RUSTUP_HOME`/`CARGO_HOME` under `C:\tools\rust`; proxies are hard links because Windows will not execute them through the symbolic links rustup prefers |

System settings: Developer Mode, long paths, Remote Desktop, no sleep or hibernation, no automatic reboot for updates
while signed in, Defender exclusions for `C:\src` and `C:\tools`, time zone copied from the host.

## Design

- **System libvirt with its own pool** (`/var/lib/libvirt/images/contr-sandbox`): libvirt's QEMU user cannot read a
  home directory with mode 750, and nothing in `$HOME` needs its permissions changed. Volumes are created owned by the
  QEMU user that libvirt reports, and attached to the VM by file path, because Ubuntu's `virt-aa-helper` adds only
  file-backed disks to the VM's AppArmor profile.
- **Fixed address** `192.168.122.211` through a DHCP reservation on the `default` network, reachable only from this host.
- **Windows 11 requirements are met, not bypassed**: Secure Boot with enrolled keys, TPM 2.0 via swtpm, a supported
  CPU passed through. `host-passthrough` also exposes SVM/VMX, which WSL 2 in the guest needs.
- **No Hyper-V enlightenments**: once WSL enables the Windows hypervisor, KVM's Hyper-V enlightenments make the guest
  hang at boot on AMD hosts. Windows runs as the root partition of its own hypervisor instead, and WSL 2 distributions
  start normally. WSL reports "nested virtualization is not supported", which only means its distributions cannot
  run VMs themselves.
- **Autologon is on**: WinUI apps and UI automation need an interactive desktop. The VM is NAT-only and its password
  stays in the user's state directory with mode 600.

## Settings

| Variable | Default |
|---|---|
| `SANDBOX_NAME` | `contr-win11` |
| `SANDBOX_MEMORY_GIB` / `SANDBOX_VCPUS` / `SANDBOX_DISK_GIB` | `8` / `6` / `128` |
| `SANDBOX_IP` / `SANDBOX_MAC` / `SANDBOX_NETWORK` | `192.168.122.211` / `52:54:00:c0:11:01` / `default` |
| `SANDBOX_POOL` | `contr-sandbox` |
| `SANDBOX_USER` | `dev` |
| `SANDBOX_STATE_DIR` | `~/.local/share/contr-sandbox` (SSH key, known hosts, password) |

## Troubleshooting

- **`wait` shows no progress for a long time**: `sandbox screenshot` and read the image, or `sandbox console`.
- **Setup never starts** (UEFI shell or "no bootable device"): the CD boot prompt timed out. Run
  `virsh --connect qemu:///system reset contr-win11` and press a key in `sandbox console`.
- **A provisioning step failed**: `sandbox wait` prints the log tail. Fix the cause, then reboot the VM; the scheduled
  task resumes at the failed step.
- **Black screen with "Display output is not active" and one vCPU at 100%** after enabling Hyper-V features: the
  domain has Hyper-V enlightenments. Remove the `<hyperv>` element (`virsh edit contr-win11`) and restart.

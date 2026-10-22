# Security review, 0.1.0-alpha.2

A review of the core before 1.0, as Phase 6 asks: where the daemon and its helpers accept input or act on the user's
behalf, what protects each place, and what changed as a result. The trust model is that the daemon acts for one user
and controls an engine that is root-equivalent on the machine it runs on, so the boundaries that matter are other
users of the same computer, code the user did not choose to run, and the network.

## Places reviewed

| Place | What protects it | Result |
|---|---|---|
| API socket on Unix | `$XDG_RUNTIME_DIR/contr`, created 0700 and refused unless the user owns it; the socket is 0600; every connection's peer UID must be the daemon's before any byte is read; systemd's socket unit uses the same modes | Sound |
| API and engine pipes on Windows | A protected DACL that allows only the user and SYSTEM, remote clients refused, and the first instance created exclusively so the daemon never shares a name someone else made | Clients did not check who owns the pipe, and `docker` cannot; fixed below |
| Discovery file | The user's runtime directory on Unix and `%LOCALAPPDATA%\Contr\run` on Windows, written 0600 | Sound |
| `contr-helper` | Run only through pkexec with `auth_admin`; acts only for the user `PKEXEC_UID` names; runs a fixed list of commands with no argument from the caller; the polkit action is bound to `/usr/libexec/contr/contr-helper` | Sound |
| Elevation on Windows | One administrator prompt, for `wsl --install` | The program was found by search, not from System32; fixed below |
| WSL distributions | Windows interop off in both, so a container that reaches the distribution cannot start Windows programs; the data distribution mounts no Windows drive | Sound |
| WSL integration | The socket in each of the user's distributions belongs to its default user and the `docker` group, mode 0660, as Docker's own is | Sound |
| `host.docker.internal` | The guest opens a stream to a Windows loopback port through the session; nothing listens on a Windows network interface | Containers reach the user's localhost services by design |
| Engine configuration | `daemon.json` keys Contr sets itself, among them `hosts`, are refused, so a setting cannot expose the engine on TCP | Sound |
| Kubernetes | The API server listens on loopback and needs the client certificate in the kubeconfig | The privileged server ran an image pulled by tag; fixed below |
| Secrets on disk | Settings may hold proxy credentials; the kubeconfig holds the cluster's client key | Written with default permissions on Unix; fixed below |
| Registry credentials | Read from the Docker CLI's configuration and helpers, sent only to the engine for the registry an image names, and left out of `Debug` output | Sound |
| Update check | HTTPS to GitHub's API with the operating system's trust store; it reports a newer release and installs nothing | Sound |
| Diagnostics bundles | Credentials, the user's name, home directory and computer name are replaced before anything enters the zip, which never leaves the computer unless the user sends it | Sound |
| Administrator policy | `HKLM\SOFTWARE\Policies\Contr` and `/etc/contr/admin-settings.json`, which only administrators can change | Sound |
| Release integrity | Guest images and bundled tools pinned by digest and SHA-256; release files attested; Linux packages, repositories and checksums signed | Windows binaries are not Authenticode-signed yet |

## Findings

| Finding | Severity | Fix |
|---|---|---|
| Pipe names are global and predictable, and clients connected to whatever served them. Another user of the same Windows computer could create a user's pipe before the daemon did and receive terminal input, settings and image requests. | Medium, on computers several people sign in to | The daemon names the user as its pipes' owner; the Rust transport and the C# SDK refuse a pipe owned by anyone else. |
| The Kubernetes server runs privileged from `rancher/k3s:<version>-k3s1`, a tag its publisher can move. A moved tag would have run with the engine's full rights. | Medium | Each supported version names its image index by digest; other versions are refused. |
| Installing WSL elevated a bare `wsl.exe`, which Windows looks for in `contrd`'s own directory first. A per-user install leaves that directory writable by the user, so a program dropped there would have been the one the prompt ran. | Low | `wsl.exe` comes from System32, for the elevated install and every other call. |
| `docker` cannot check who owns the engine pipe the `contr` context names, and that name was the same on every run. Another user who took it first would have kept the engine from starting, while a `contr` context left from an earlier run still led `docker` to their pipe. | Low, on computers several people sign in to | The engine pipe's name ends in 16 random bytes, new each time the daemon starts. Only the daemon's user can read the `contr` context that names it. |
| The settings file and the kubeconfig's staging copy were written with the default permissions, readable by other users where home directories are. | Low | Both are created 0600. |

## Open

- Windows binaries stay unsigned until SignPath Foundation accepts Contr; see the
  [code signing policy](code-signing-policy.md).

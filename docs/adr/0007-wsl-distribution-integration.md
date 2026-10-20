# 0007. The user's WSL distributions reach the engine through a relayed socket and a shared root mount

## Context

Developers on Windows often work inside their own WSL distributions and expect `docker` there to use the same engine
as Windows. Paths they bind name files of their distribution, which the engine's distribution cannot see. Every WSL 2
distribution runs in one VM, and mounts under `/mnt/wsl` propagate to all of them.

## Decision

- `dockerd` also listens on `/mnt/wsl/contr/docker.sock`, which only root may use.
- The runtime agent copies itself, the Docker CLI, Compose and Buildx onto the data distribution's storage once per
  release. Integrated distributions run them from there, so nothing is installed into them and no memory-backed
  directory holds the binaries.
- For each integrated distribution, `contrd` runs `contr-agent integrate` as root inside it while the engine runs. The
  agent bind-mounts the distribution's root file system at `/mnt/wsl/contr/distros/<name>`, serves
  `/var/run/docker.sock` to the distribution's default user and `docker` group through `contr-engine-proxy`, and links
  the Docker CLI into `/usr/local` where the distribution has no Docker package of its own.
- The proxy moves absolute bind sources under the distribution's mount; `/mnt` paths, which every distribution
  shares, and the engine socket stay as written.
- The list of integrated distributions is a per-user setting. An integration that ends is restarted with a growing
  delay, which also restarts a distribution the user terminated.

## Consequences

- `docker` and Compose in an integrated distribution work without configuration, including relative bind mounts.
- Submounts of the distribution, such as `/run` or a custom drive root, are not visible to bind mounts.
- A distribution that already serves `/var/run/docker.sock` is reported as failed and left untouched.
- An integration killed without cleanup leaves its root mount in every distribution; `contrd` releases it from the
  runtime distribution, and the agent releases all such mounts when the engine starts. Terminating or unregistering
  the distribution meanwhile succeeds; the stale mount then fails with I/O errors.
- Integrated distributions keep running while the engine runs, so WSL does not stop them when idle.

# Specifications

Specifications describe what Contr must do. Implementers build from these files only; they never read research
material, analysis output or the products that were studied.

## Sources (lanes)

| Lane | Allowed sources |
|---|---|
| A | Apache-2.0 and MIT projects: Moby, Docker CLI, Compose, Buildx, Rancher Desktop, Podman Desktop, container-desktop, Lima, k3s, k3d |
| B | Public documentation, paraphrased and cited by URL |

Docker Desktop is never a source: its license forbids reverse engineering and building a competing product from it.
Questions only it could answer become product decisions, recorded as such.

## Requirement format

```markdown
### REQ-CONTAINERS-001: Stop a running container
Lane: A | Evidence: ev_<id>, ev_<id> | Obligation: obl_<id>

Given a running container
When the user stops it
Then the engine receives a stop request with the configured timeout
And the container list shows it as exited within 2 seconds of the engine event
```

- IDs are `REQ-<AREA>-<NNN>` and never reused.
- Evidence IDs are opaque references into the private research store; specs never quote it.
- Acceptance criteria are observable: CLI output, API calls, UI state.

## Rules enforced in review and CI

- No screenshots, images, copied text longer than a short phrase, or paths and process names of studied proprietary
  products.
- Every requirement has a lane and at least one acceptance criterion.
- `specs/**` changes need approval from a clean-room reviewer (CODEOWNERS).

## Files

| File | Contents |
|---|---|
| [000-feature-inventory.md](000-feature-inventory.md) | Every feature, its release and its spec |
| `ux/` | One file per screen or flow |
| `settings/` | Settings catalog: key, type, default, platform, restart needs, admin lockability |
| `engine/` | Lifecycle state machine and backend behavior |
| `cli/` | `contr` command reference |
| `compat/` | Docker CLI scenarios that must match plain Moby |

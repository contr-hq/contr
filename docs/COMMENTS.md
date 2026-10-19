# Comments: the convention

Code explains itself through names, types and small functions. A comment states a fact about the current code that the
code cannot show: a constraint, an invariant, the consequence of breaking it, a wire contract, a security decision, or a
platform trap. Nothing else.

This file is identical in every `contr-hq` repository and follows the convention of
[kakehashi](https://github.com/SekiroKenjii/kakehashi/blob/development/docs/COMMENTS.md).

## Rules

1. **Facts, not narrative.** No rhetoric, metaphors or conversation with a reviewer. After reading a comment you can
   answer "what does this require or forbid?" If you cannot, delete it.
2. **Present tense, current code only.** History belongs in the pull request or an ADR. A comment containing *used to*,
   *no longer*, *the old*, *until now* or *previously* fails CI.
3. **Density follows danger.** Comment where a mistake is expensive and invisible:
   - contract surfaces: `proto/`, `contr-core`, `contr-backend`, `contr-transport`, `contr-sdk`, C#
     `*.UI.Contracts` / `*.Application.Abstractions`, Linux `*-api` crates;
   - anything that crosses the wire: say in one sentence that renaming it breaks deployed clients;
   - security decisions: pipe ACLs, peer credentials, helper allow-lists, proxy rewrites;
   - platform traps: WSL, named pipes, UI-thread affinity, socket activation.

   A member whose name says everything gets no comment.
4. **Long comments are misplaced documents.** Beside a statement the limit is two lines, and CI enforces it. A block over
   about six lines belongs in `docs/` or `docs/adr/NNNN-<slug>.md`, leaving one sentence and a link. A comment that
   carries a link says what the thing is, then links.
5. **Docs are the source of truth.** Comments may link to docs; docs never cite comments.

## Shape

| Language | Declarations | Inside bodies | Required docs |
|---|---|---|---|
| Rust | `///` on items, `//!` module docs of 1–3 sentences | `//`, at most two lines | `#![warn(missing_docs)]` on contract crates only |
| C# | `///`, never `//`; `<summary>` is one sentence, `<remarks>` only for an invariant (≤ 4 lines) | `//`, at most two lines | CS1591 on contract assemblies only |
| Proto | `//` on every service and RPC | — | buf `COMMENT_SERVICE`, `COMMENT_RPC` |
| Swift | `///` on public SDK API | `//`, at most two lines | public `ContrSDK` API only |

`unsafe` blocks carry a `// SAFETY:` comment stating why the invariants hold.

## Examples

Wrong: the name already says it.

```rust
/// Whether the engine is running.
pub fn is_running(&self) -> bool
```

Right: no comment.

```rust
pub fn is_running(&self) -> bool
```

Right: a wire contract, said once.

```proto
// Sent in google.rpc.ErrorInfo.reason as the value name without the ERROR_REASON_ prefix.
// Clients branch on these names, so renaming or renumbering a value breaks deployed clients.
enum ErrorReason {
```

Right: a platform trap at the line where it bites.

```rust
// wsl.exe writes UTF-16LE to stdout; decode before matching.
let text = String::from_utf16_lossy(&units);
```

## Enforcement

The `comment-lint` workflow in [contr-hq/.github](https://github.com/contr-hq/.github) runs on every pull request:

- history words in any comment fail;
- a `//` run directly above a Rust or C# declaration fails (it must be `///`);
- a `//` block of three or more lines fails.

Generated code (`gen/`, `obj/`, `bin/`, `target/`) is excluded. Everything else is review. When you touch a file for
another reason, only the comments in your change must conform.

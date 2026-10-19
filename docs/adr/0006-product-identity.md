# 0006. Product-derived names come from one module

## Context

The working name changed once already because of a trademark conflict. Pipe names, socket paths, WSL distributions,
contexts, package IDs, application IDs and polkit actions all derive from the product name and must stay consistent
across the daemon, agent and clients.

## Decision

- `contr_core::product` defines every product-derived identifier. Code never spells them out elsewhere.
- Client repositories mirror the same values in one constants file each, checked against the daemon's `Handshake`.
- Docker marks are used only to describe compatibility, never in product, package or domain names.

## Consequences

A rename is a change to `product.rs` plus each client's constants file. A trademark search for "Contr" is required
before the first public release.

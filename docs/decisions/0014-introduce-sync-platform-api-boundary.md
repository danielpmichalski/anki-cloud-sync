# 14. Introduce sync-platform-api as a stable platform boundary

Date: 2026-04-23

## Status

Accepted

## Context

The sync server originally bundled two concerns in a single binary:

1. **Core sync protocol** — `rslib/` (upstream, verbatim), `sync-storage-backends/` (Google Drive
   + local impls), `sync-storage-server/` (composition root). These are inherently platform-agnostic.

2. **Platform-specific glue** — `sync-storage-config/` (SQLite queries, AES-256-GCM token
   decryption, bcrypt credential check, OAuth token exchange). This is specific to the `anki-cloud`
   deployment's DB schema and credential storage mechanism.

This bundling blocked two planned deployment targets:

- **anki-cloud** (cloud Docker): needs a DB-backed `AuthProvider` and a Drive-backed `BackendResolver`
  that knows about its own SQLite schema and AES key format.
- **anki-cloud-android** (embedded JVM service): needs Room-backed auth and Android Credential
  Manager for token storage — completely different from the cloud stack.

The `AuthProvider`, `BackendResolver`, and `StorageBackend` trait definitions already existed in
`sync-storage-api` and were injected into `rslib` at startup. They formed a natural seam.

## Decision

Formalise the trait boundary as a dedicated crate named `sync-platform-api`, and remove all
platform-specific knowledge from this repository:

1. **Rename `sync-storage-api` → `sync-platform-api`** to signal its role as the stable external
   contract. External repos `use sync_platform_api::*`; this repo's internals never need to change.

2. **Delete `CloudAuthProvider` and `CloudBackendResolver`** from `sync-storage-server`. The
   standalone binary published from this repo runs in standalone mode only (env-var users, local
   filesystem). Cloud deployments supply their own implementations externally.

3. **Remove `SyncMode` enum and `mode_from_env()`** from `sync-storage-server`. With only one
   built-in mode, the enum is dead weight. `make_providers()` always returns the standalone pair.

4. **Remove `sync-storage-config` as a dependency of `sync-storage-server`**. The crate stays in
   the workspace temporarily (as a reference implementation for `anki-cloud` to port from) but is
   no longer compiled into the binary. It will be deleted once `anki-cloud` owns its own
   `sync-platform-cloud` crate.

After this change:
- `anki-cloud-sync` has zero knowledge of any DB schema, AES key format, JNI, or Android specifics.
- `anki-cloud` owns `sync-platform-cloud` (DB auth + OAuth + AES).
- `anki-cloud-android` owns `sync-platform-android` (Room + Android Credential Manager + JNI).

## Consequences

**Positive:**

- Clear ownership: this repo owns the sync protocol and the trait boundary; platform repos own their
  auth and storage glue.
- Multiple deployment targets can share the same `sync-platform-api` crate without forking this repo.
- Upstream upgrades to `rslib` require no changes to platform crates.
- The Docker image published from this repo is a clean standalone server with no cloud dependencies.

**Negative / transitional:**

- `sync-storage-config` temporarily remains in the workspace as a dead dependency. This is
  intentional — it serves as the reference for `anki-cloud`'s port. It will be removed in a
  follow-up once the port is complete.
- The published Docker image no longer supports cloud deployments directly. Platform teams must
  build their own binary that links `sync-platform-api` + their own platform crate.

## References

- TODO.md §[ARCH] Introduce sync-platform-api — clean platform boundary
- [ADR-0003](0003-fork-rust-ankitects-sync-server.md) — original fork decision and storage abstraction rationale

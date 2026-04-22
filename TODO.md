# TODO

---

## [ARCH] Introduce sync-platform-api — clean platform boundary

### Background

The sync server currently bundles two concerns that must be separated to support multiple
deployment targets (cloud Docker, Android embedded service):

1. **Core sync protocol** — `rslib/` (upstream, verbatim), `sync-storage-backends/` (Google Drive
   + local impls), `sync-storage-server/` (composition root). These are platform-agnostic.

2. **Platform-specific glue** — `sync-storage-config/` (SQLite queries, AES-256-GCM token
   decrypt, bcrypt auth, OAuth HTTP exchange). This knows about a specific DB schema and
   credential storage mechanism. It must be extracted.

The trait boundary already exists in `sync-storage-api`: `AuthProvider`, `BackendResolver`,
`StorageBackend`. This task formalises it as `sync-platform-api` — the **only** public contract
that external deployment targets depend on — and strips all platform knowledge from this repo.

After this task:
- `anki-cloud-sync` knows nothing about SQLite schemas, AES keys, JNI, or Android.
- `anki-cloud` owns its own `sync-platform-cloud` crate (SQLite + OAuth + AES).
- `anki-cloud-android` owns its own `sync-platform-android` crate (Room + Android Credential
  Manager + JNI callbacks).

### Tasks

**1. Rename `sync-storage-api` → `sync-platform-api`**
- Rename directory and crate name in `sync-platform-api/Cargo.toml`
- Update workspace `Cargo.toml`: replace `sync-storage-api` entry with `sync-platform-api`
- Update all import paths across the workspace:
  - `sync-storage-backends/` — `use sync_storage_api::*` → `use sync_platform_api::*`
  - `sync-storage-server/` — same
  - `rslib/src/sync/http_server/mod.rs` — same
  - `rslib/src/sync/http_server/user.rs` — same
- No logic changes. Traits are already correct as-is.

**2. Delete `sync-storage-config` crate**
- Coordinate with `anki-cloud` team: `sync-platform-cloud` must land there first (it takes
  ownership of all DB queries, token decryption, OAuth exchange, and bcrypt auth currently
  in `sync-storage-config`).
- Once `anki-cloud` is ready:
  - Remove `sync-storage-config/` directory
  - Remove from workspace `Cargo.toml`
  - Remove from `sync-storage-server/Cargo.toml` dependencies

**3. Strip Cloud impls from `sync-storage-server`**
- In `sync-storage-server/src/auth.rs`: delete `CloudAuthProvider`
- In `sync-storage-server/src/resolver.rs`: delete `CloudBackendResolver`
- In `sync-storage-server/src/lib.rs`:
  - Delete `SyncMode::Cloud` variant and its `make_providers()` branch
  - Remove `sync-storage-config` import
- `sync-storage-server` retains only `StandaloneAuthProvider` + `StandaloneBackendResolver`
  (env-var user list, no DB, no OAuth).

**4. Update CLAUDE.md**
- Update crate table to reflect new names
- Add section explaining `sync-platform-api` as the public contract for external impls
- Clarify that this repo has zero knowledge of any DB schema, JNI, or Android

### Acceptance criteria
- `cargo build --bin anki-sync-server` succeeds (standalone mode)
- `cargo test -p sync-platform-api` passes
- `cargo test -p sync-storage-backends` passes
- No `sync_storage_config` imports anywhere in the workspace
- No JNI or SQLite schema references anywhere in the workspace

---

## [BACKLOG] Expose `SimpleServer` as a stable library interface

Currently `SimpleServer::new(base_folder, auth, resolver)` is in `rslib` (upstream, no-edit).
For external callers (anki-cloud-android's `sync-server-jni`) to call it, a thin shim crate
may be needed that re-exports it with a stable API surface.

Defer until anki-cloud-android needs it — implement only if the existing import path is
impractical from an external crate.

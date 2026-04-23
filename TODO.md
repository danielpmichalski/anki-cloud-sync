# TODO

---

## [ARCH] Introduce sync-platform-api — clean platform boundary

### Background

The sync server originally bundled two concerns that must be separated to support multiple
deployment targets (cloud Docker, Android embedded service):

1. **Core sync protocol** — `rslib/` (upstream, verbatim), `sync-storage-backends/` (Google Drive
   + local impls), `sync-storage-server/` (composition root). These are platform-agnostic.

2. **Platform-specific glue** — `sync-storage-config/` (SQLite queries, AES-256-GCM token
   decrypt, bcrypt auth, OAuth HTTP exchange). This knows about a specific DB schema and
   credential storage mechanism. It must be extracted.

The trait boundary already exists in `sync-platform-api`: `AuthProvider`, `BackendResolver`,
`StorageBackend`. This is the **only** public contract that external deployment targets depend on.

After this refactor:
- `anki-cloud-sync` knows nothing about SQLite schemas, AES keys, JNI, or Android.
- `anki-cloud` owns its own `sync-platform-cloud` crate (SQLite + OAuth + AES).
- `anki-cloud-android` owns its own `sync-platform-android` crate (Room + Android Credential
  Manager + JNI callbacks).

### Tasks

**1. ✅ Rename `sync-storage-api` → `sync-platform-api`**

- Renamed directory and crate name in `sync-platform-api/Cargo.toml`
- Updated workspace `Cargo.toml`: replaced `sync-storage-api` entry with `sync-platform-api`
- Updated all import paths across the workspace (9 files)

**2. Delete `sync-storage-config` crate**

Blocked on `anki-cloud` team landing `sync-platform-cloud` first (it takes ownership of all DB
queries, token decryption, OAuth exchange, and bcrypt auth currently in `sync-storage-config`).

Sequencing:

```
anki-cloud-sync (rename + strip) → TAG
├── anki-cloud (sync-platform-cloud)           ┐ parallel
└── anki-cloud-android (sync-platform-android) ┘
↓
anki-cloud-sync (delete sync-storage-config) → TAG
↓
anki-cloud (bump pinned tag)
```

Once `anki-cloud` signals ready:
- Remove `sync-storage-config/` directory
- Remove from workspace `Cargo.toml`
- Remove from `sync-storage-server/Cargo.toml` dependencies

**3. ✅ Strip Cloud impls from `sync-storage-server`**

- Deleted `CloudAuthProvider` from `sync-storage-server/src/auth.rs`
- Deleted `CloudBackendResolver` from `sync-storage-server/src/resolver.rs`
- Removed `SyncMode` enum, `mode_from_env()`, and Cloud branch from `sync-storage-server/src/lib.rs`
- Removed `sync-storage-config` dep from `sync-storage-server/Cargo.toml`
- `sync-storage-server` now retains only `StandaloneAuthProvider` + `StandaloneBackendResolver`

**4. ✅ Update docs**

- Updated CLAUDE.md, README.md, TODO.md, added ADR-0014

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

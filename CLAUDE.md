# CLAUDE.md — anki-cloud-sync

> Authoritative reference for AI assistants and contributors working on this repository.
> Covers architecture, decisions, build mechanics, and contribution rules.

---

## 1. What This Repository Is

A **platform-neutral Anki sync server** — a fork of the Rust sync server built into
[ankitects/anki](https://github.com/ankitects/anki) at tag `25.09`, plus four custom crates
that define the adapter trait boundary and one built-in implementation.

The server exposes a stable trait boundary (`sync-platform-api`) so external platform crates can
supply their own `AuthProvider` and `BackendResolver` implementations without touching this repo.
This repo ships one built-in implementation: **standalone mode** (env-var users, local filesystem),
suitable for self-hosting and local development.

The sync server is published as an **external Docker image** consumed by the `anki-cloud` platform.
It has no knowledge of the REST API, MCP server, or web UI — it only knows about:

- The Anki sync protocol (upstream, unmodified)
- The `sync-platform-api` trait boundary (AuthProvider, BackendResolver, StorageBackend)
- Cloud storage backends (Google Drive, local) — via `sync-storage-backends`

### Trademark note

**"Anki" is a registered trademark** of Ankitects Pty Ltd. Do NOT use "Anki" in any product
name or marketing copy. This repository name (`anki-cloud-sync`) refers to the sync protocol,
not to Ankitects' product.

---

## 2. Repository Structure

```
/
├── CLAUDE.md                       ← this file
├── Cargo.toml                      ← workspace root (only non-upstream file at root)
├── Cargo.lock                      ← copied from upstream ankitects/anki@25.09
├── Dockerfile                      ← multi-stage build → published Docker image
├── .version                        ← Anki version this is pinned to (e.g. "25.09")
├── README.md
│
├── sync-platform-api/              ← OUR CRATE: stable public trait boundary
│   └── src/lib.rs                  ← AuthProvider, BackendResolver, StorageBackend
│
├── sync-storage-backends/          ← OUR CRATE: backend factory + implementations
│   └── src/
│       ├── lib.rs                  ← StorageBackendFactory
│       └── backends/
│           ├── mod.rs
│           ├── local.rs            ← no-op backend (dev/self-hosting)
│           └── google_drive.rs     ← Google Drive implementation
│
├── sync-storage-config/            ← OUR CRATE: DB lookups, token decryption, bcrypt auth
│   └── src/lib.rs                  ← TRANSITIONAL: moves to anki-cloud's sync-platform-cloud
│
├── sync-storage-server/            ← OUR CRATE: composition root (standalone mode)
│   └── src/
│       ├── lib.rs                  ← make_providers() → standalone pair
│       ├── auth.rs                 ← StandaloneAuthProvider (SYNC_USER* env vars + PBKDF2)
│       ├── resolver.rs             ← StandaloneBackendResolver (local filesystem)
│       └── sidecar.rs              ← InternalServer (optional sidecar HTTP API)
│
├── rslib/                          ← VERBATIM UPSTREAM (ankitects/anki@25.09 rslib/)
│   ├── Cargo.toml
│   ├── sync/
│   │   ├── Cargo.toml             ← binary crate: `anki-sync-server`
│   │   └── main.rs                ← entry point
│   └── src/sync/http_server/      ← ADR-0003 hook points (fetch/commit) wired here
│
├── ftl/                            ← VERBATIM UPSTREAM (Fluent translations + submodules)
├── proto/                          ← VERBATIM UPSTREAM (protobuf definitions)
│
└── scripts/
    └── fork-anki-sync-server.zsh  ← re-sync rslib/ from upstream at a new tag
```

**Critical rule:** Never edit anything inside `rslib/`, `ftl/`, or `proto/` by hand.
Those are verbatim upstream copies. Changes go in the four custom crates only.
Exception: mechanical import-path updates (`use sync_platform_api::` etc.) when renaming our
crates — these are unavoidable and do not touch protocol logic.
To upgrade upstream, run `scripts/fork-anki-sync-server.zsh <new-tag>`.

---

## 3. Custom Crates vs. Upstream

| Crate                   | Files                          | Purpose                                                                                                          | Edit?  |
|-------------------------|--------------------------------|------------------------------------------------------------------------------------------------------------------|--------|
| `sync-platform-api`     | `src/lib.rs` (~30 lines)       | Stable public contract: `AuthProvider`, `BackendResolver`, `StorageBackend` — external impls depend on this      | Yes    |
| `sync-storage-backends` | `src/lib.rs` + `backends/*.rs` | Factory + per-provider storage implementations (Google Drive, local)                                             | Yes    |
| `sync-storage-config`   | `src/lib.rs`                   | **Transitional** — DB lookups, AES-256-GCM token decryption, bcrypt auth, OAuth exchange. Moving to `anki-cloud` | Yes*   |
| `sync-storage-server`   | `src/*.rs`                     | Composition root: standalone auth + resolver, optional sidecar server                                            | Yes    |
| `rslib` and sub-crates  | all files                      | Upstream Anki sync protocol + binary                                                                             | **No** |
| `ftl/`, `proto/`        | all files                      | Upstream build deps                                                                                              | **No** |

*`sync-storage-config` will be deleted once `anki-cloud` has its own `sync-platform-cloud` crate.

---

## 4. Architecture

### 4.1 Platform-API Traits (`sync-platform-api`)

These three traits are the **only interface** between upstream rslib and any deployment target.
External platform crates implement them; rslib imports them; this repo's built-in standalone
implementation lives in `sync-storage-server`.

```rust
pub trait AuthProvider: Send + Sync {
    /// Validate credentials. Returns `(hkey, email)` on success.
    fn authenticate(&self, username: &str, password: &str) -> Result<(String, String)>;

    /// Reverse-lookup: `hkey` → `email`. Called once per authenticated request.
    fn lookup_by_hkey(&self, hkey: &str) -> Result<String>;
}

pub trait BackendResolver: Send + Sync {
    fn resolve_for_user(&self, username: &str) -> Result<Box<dyn StorageBackend>>;
}

pub trait StorageBackend: Send + Sync {
    /// Download user's collection to `dest` before sync begins.
    fn fetch(&self, user: &str, dest: &Path) -> Result<()>;

    /// Upload user's collection from `src` after sync completes.
    fn commit(&self, user: &str, src: &Path) -> Result<()>;
}
```

### 4.2 Request Lifecycle

The server is wired at startup with concrete `AuthProvider` and `BackendResolver` instances.
At runtime each request flows through them:

```
1. Anki client → POST /sync/{method}
2. Axum router (rslib/src/sync/http_server/routes.rs)
3. SyncProtocol::with_authenticated_user()
   → auth.lookup_by_hkey(hkey)
     Standalone: in-memory map (populated from SYNC_USER* at startup)
     Platform:   DB query on users_sync_state.sync_key
4. User::open_collection()
   → resolver.resolve_for_user(email) → Box<dyn StorageBackend>
     Standalone: StorageBackendFactory::create("local", …) → no-op
     Platform:   DB lookup + OAuth token exchange → StorageBackendFactory::create("google", …)
   → backend.fetch(user, dest)
     Standalone: no-op (collection already on local disk)
     Platform:   download collection.anki2 from Google Drive
5. Sync operations run against local SQLite copy of collection
6. backend.commit(user, src)
     Standalone: no-op
     Platform:   upload collection.anki2 back to Google Drive
```

**Key property:** Fully stateless per request. Each request re-resolves auth and storage from
scratch. Safe for horizontal scaling (platform impls must ensure the same).

### 4.3 Authentication

**Standalone mode** (built into this binary): authenticates via `SYNC_USER*` env vars + PBKDF2.

```
POST /sync/hostKey {username: email, password: sync_password}
→ lookup (username:password) in in-memory SYNC_USER* map
→ PBKDF2.verify(password, stored_hash)
→ hkey = SHA1(username:password)
→ return {key: hkey}

Subsequent requests carry hkey in anki-sync header.
On restart: hkey not in memory → re-derive from SYNC_USER* map (deterministic).
```

**Platform implementations** supply their own `AuthProvider`. A cloud implementation
(e.g. `anki-cloud`'s `sync-platform-cloud`) uses bcrypt against a DB and persists hkeys:

```
→ bcrypt.verify(password, users.sync_password_hash)
→ hkey = SHA1(email:password)
→ upsert users_sync_state SET sync_key = hkey WHERE user_id = ...
On restart/failover: hkey not in memory → lookup_user_by_sync_key(hkey) → re-hydrate.
```

### 4.4 Token Encryption (platform layer concern)

OAuth refresh tokens are stored **encrypted at rest** (AES-256-GCM) in `storage_connections.oauth_refresh_token`.

Format: `base64url(IV[12 bytes] || ciphertext+tag)`

Currently implemented in `sync-storage-config::decrypt_token()` — **transitional**. Once
`anki-cloud` owns `sync-platform-cloud`, this logic moves there and must remain byte-for-byte
compatible with the TypeScript `encrypt()`/`decrypt()` in `packages/db/src/encrypt.ts`.

Encryption key: `TOKEN_ENCRYPTION_KEY` env var — 32 bytes as 64 hex chars or 44 base64 chars.

### 4.5 Google Drive Backend

`sync-storage-backends/src/backends/google_drive.rs` — key details:

- **Folder**: creates/finds `"AnkiSync"` folder in user's Drive root
- **File**: `"collection.anki2"` inside that folder
- **Download**: exponential backoff on 403/429 (formula: `2^attempt + jitter_ms`, max 6 attempts, cap 32s)
- **Upload**: resumable upload, 256 KB chunks
- Uses `tokio::task::block_in_place()` to call async from sync context (upstream rslib is sync)

### 4.6 Local Backend

`sync-storage-backends/src/backends/local.rs` — no-op.
`fetch()` and `commit()` do nothing. Collection lives in `SYNC_BASE` temp dir.
Used for local development and self-hosting without cloud storage.

---

## 5. Environment Variables

All variables listed here apply to the **standalone binary** published from this repo.
Platform-specific vars (DATABASE_URL, TOKEN_ENCRYPTION_KEY, GOOGLE_CLIENT_*) belong to
the external platform crate, not this binary.

| Variable              | Default         | Description                                                         |
|-----------------------|-----------------|---------------------------------------------------------------------|
| `SYNC_BASE`           | `~/.syncserver` | Temp directory for user collections during sync                     |
| `SYNC_HOST`           | `0.0.0.0`       | Bind address                                                        |
| `SYNC_PORT`           | `8080`          | Bind port                                                           |
| `SYNC_USER1`          | —               | `username:password` — repeat as `SYNC_USER2`, `SYNC_USER3`, …       |
| `SYNC_INTERNAL_TOKEN` | —               | Bearer token for internal API; if unset, sidecar server is disabled |
| `SYNC_INTERNAL_HOST`  | `127.0.0.1`     | Bind address for internal API                                       |
| `SYNC_INTERNAL_PORT`  | `8081`          | Port for internal API                                               |
| `RUST_LOG`            | `anki=info`     | Log level (tracing filter)                                          |

`SYNC_*` vars are loaded via `envy::prefixed("SYNC_")` into `SyncServerConfig`.

---

## 6. Build

### Prerequisites

- Rust ≥ 1.80 (MSRV)
- `protobuf-compiler` (`apt install protobuf-compiler` or `brew install protobuf`)
- `pkg-config` + `libssl-dev` (Linux) or `openssl` (macOS via Homebrew)

### Build commands

```bash
# debug build
cargo build --bin anki-sync-server

# release build
cargo build --release --bin anki-sync-server

# run tests (custom crates only — upstream tests are not our concern)
cargo test -p sync-platform-api
cargo test -p sync-storage-backends
cargo test -p sync-storage-server
```

### Docker

```bash
# build image
docker build -t anki-cloud-sync:local .

# run (standalone mode — no DB or cloud credentials required)
docker run \
  -e SYNC_USER1=alice@example.com:secret \
  -p 8080:8080 \
  anki-cloud-sync:local
```

Healthcheck: `anki-sync-server --healthcheck` (exits 0 if running, 1 otherwise).
Used by Docker and docker-compose health checks.

---

## 7. Versioning

Tags follow the format **`v<anki-version>-r<revision>`** (e.g. `v25.09-r1`).

- The Anki version prefix signals sync protocol compatibility.
- `-rX` is our revision counter for changes on top of that upstream base.
- `-rX` resets to `-r1` whenever the upstream Anki version changes.
- Anki uses `25.09`, `25.09.1`, `25.09.2` style — the `-r` suffix avoids collision.

**Examples:**

| Event                          | Tag           |
|--------------------------------|---------------|
| Initial release on Anki 25.09  | `v25.09-r1`   |
| Add Dropbox backend            | `v25.09-r2`   |
| Add S3 backend                 | `v25.09-r3`   |
| Upgrade to Anki 25.09.3        | `v25.09.3-r1` |
| Add OneDrive on top of 25.09.3 | `v25.09.3-r2` |
| Upgrade to Anki 25.10          | `v25.10-r1`   |

A compatibility table in README maps Anki client versions to sync-server image tags.

When Anki releases a new version, the upgrade path is:

1. `./scripts/fork-anki-sync-server.zsh <new-tag>` — re-syncs rslib/ from upstream
2. Verify custom crates still compile against updated rslib
3. Run tests
4. Tag new release resetting revision (e.g. `v25.10-r1`)

---

## 8. Upstream Upgrade Process

```bash
# re-sync rslib/, ftl/, proto/ from upstream at a given tag
./scripts/fork-anki-sync-server.zsh 25.10

# after syncing:
cargo build --bin anki-sync-server   # verify it compiles
cargo test -p sync-storage-backends  # verify custom crates still work
cargo test -p sync-storage-server
```

**Never manually edit** `rslib/`, `ftl/`, or `proto/`.
If upstream breaks our integration points, fix by adapting the custom crates, not by patching rslib.

---

## 9. Integration with anki-cloud Monorepo

This repo publishes a Docker image containing the **standalone sync server binary**.
The `anki-cloud` platform builds a **separate binary** that depends on `sync-platform-api`
and links in its own `sync-platform-cloud` crate (DB auth + OAuth token exchange + AES decryption).

**Schema contract (platform layer reads/writes, not this binary):**

- Reads from: `users`, `storage_connections`, `users_sync_state` tables
- Writes to: `users_sync_state.sync_key` (upsert on auth)
- Never reads or writes: `api_keys` table
- Schema migrations are owned by `packages/db` in the monorepo — the platform crate is a consumer, not the schema owner

---

## 10. Key Design Principles

1. **Never store deck data.** Collections pass through (temp file during sync), uploaded to user's cloud storage, then deleted locally.
2. **Stateless per request.** No in-memory state that can't be re-derived from auth/storage config. Safe for restart and horizontal scale.
3. **`sync-platform-api` is the stable external contract.** External deployment targets (`anki-cloud`, `anki-cloud-android`) implement `AuthProvider`, `BackendResolver`, and `StorageBackend`. This repo has zero knowledge
   of any DB schema, JNI, or Android specifics.
4. **Custom crates are thin adapters.** They translate between the upstream rslib interfaces and external systems. Keep them small.
5. **Upstream is upstream.** `rslib/`, `ftl/`, `proto/` are verbatim copies. No hand-edits, ever (exception: mechanical crate-import renames when our crate names change).
6. **Conventional commits.** Required for automated changelog and semantic release.
7. **AI Agents: Never auto-commit code.** Inform user that changes are ready; let user handle git commits themselves.

---

## 11. CI/CD

GitHub Actions workflows:

- **`ci.yml`** — runs on every push/PR: `cargo build`, `cargo test -p sync-platform-api`, `cargo test -p sync-storage-*`, Docker build
- **`release.yml`** — release-please + conventional commits → auto-bumps version, publishes Docker image to `ghcr.io/danielpmichalski/anki-cloud-sync:<tag>`

Docker image published to: `ghcr.io/danielpmichalski/anki-cloud-sync`

---

*Last updated: 2026-04-23.*

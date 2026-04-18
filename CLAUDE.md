# CLAUDE.md — anki-cloud-sync

> Authoritative reference for AI assistants and contributors working on this repository.
> Covers architecture, decisions, build mechanics, and contribution rules.

---

## 1. What This Repository Is

A **modified Anki sync server** that stores user collections in user-owned cloud storage
(Google Drive, Dropbox, S3) instead of the local filesystem. Extracted from the
[anki-cloud](https://github.com/danielpmichalski/anki-cloud) monorepo.

It is a **fork of the Rust sync server** built into [ankitects/anki](https://github.com/ankitects/anki)
at tag `25.09`, plus three custom crates that implement the cloud storage adapter layer.

The sync server is consumed by the wider `anki-cloud` platform as an **external Docker image**.
It has no knowledge of the REST API, MCP server, or web UI — it only knows about:

- The Anki sync protocol (upstream, unmodified)
- SQLite (shared with the rest of the platform; read-only from this server's perspective)
- Cloud storage backends (Google Drive, local)

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
├── sync-storage-api/               ← OUR CRATE: StorageBackend trait
│   └── src/lib.rs
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
│   └── src/lib.rs
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
Those are verbatim upstream copies. Changes go in the three custom crates only.
To upgrade upstream, run `scripts/fork-anki-sync-server.zsh <new-tag>`.

---

## 3. Custom Crates vs. Upstream

| Crate                   | Files                          | Purpose                                                                                 | Edit?  |
|-------------------------|--------------------------------|-----------------------------------------------------------------------------------------|--------|
| `sync-storage-api`      | `src/lib.rs` (12 lines)        | `StorageBackend` trait — the only interface between upstream and our code               | Yes    |
| `sync-storage-backends` | `src/lib.rs` + `backends/*.rs` | Factory + per-provider implementations                                                  | Yes    |
| `sync-storage-config`   | `src/lib.rs`                   | DB lookups, AES-256-GCM token decryption, bcrypt credential check, OAuth token exchange | Yes    |
| `rslib` and sub-crates  | all files                      | Upstream Anki sync protocol + binary                                                    | **No** |
| `ftl/`, `proto/`        | all files                      | Upstream build deps                                                                     | **No** |

The three custom crates are minimal and deliberately decoupled so upstream upgrades
don't require touching our code.

---

## 4. Architecture

### 4.1 StorageBackend Trait

```rust
pub trait StorageBackend: Send + Sync {
    /// Download user's collection to `dest` before sync begins.
    fn fetch(&self, user: &str, dest: &Path) -> Result<()>;

    /// Upload user's collection from `src` after sync completes.
    fn commit(&self, user: &str, src: &Path) -> Result<()>;
}
```

This is the **entire interface** between upstream rslib and cloud storage.
All storage complexity lives behind `fetch` and `commit`.

### 4.2 Request Lifecycle

```
1. Anki client → POST /sync/{method}
2. Axum router (rslib/src/sync/http_server/routes.rs)
3. SyncProtocol::with_authenticated_user()
   → validates hkey: first check in-memory map, then DB (users_sync_state.sync_key)
4. User::open_collection()
   → sync_storage_config::fetch_storage_connection(email)
       ← SELECT provider, oauth_refresh_token FROM storage_connections JOIN users WHERE email = ?
   → sync_storage_config::exchange_refresh_token(refresh_token)  [if provider != "local"]
       ← POST https://oauth2.googleapis.com/token
   → StorageBackendFactory::create(provider, access_token)
   → backend.fetch(user, dest)  [downloads collection from GDrive]
5. Sync operations run against local SQLite copy of collection
6. backend.commit(user, src)  [uploads collection back to GDrive]
```

**Key property:** Fully stateless per request. Every request independently fetches storage
config from DB and exchanges a fresh OAuth access token. Safe for horizontal scaling.

### 4.3 Authentication

Anki clients authenticate with **email + sync password** (not Google OAuth).
The sync password is a separate credential generated in the web UI and stored as a
bcrypt hash in `users.sync_password_hash`.

```
POST /sync/hostKey {username: email, password: sync_password}
→ bcrypt.verify(password, users.sync_password_hash)  [timing-safe; always runs even for unknown users]
→ hkey = SHA1(email:password)
→ upsert users_sync_state SET sync_key = hkey WHERE user_id = ...
→ return {key: hkey}

Subsequent requests carry hkey in anki-sync header.
On restart/failover: hkey not in memory → lookup_user_by_sync_key(hkey) → re-hydrate.
```

### 4.4 Token Encryption

OAuth refresh tokens are stored **encrypted at rest** (AES-256-GCM) in `storage_connections.oauth_refresh_token`.

Format: `base64url(IV[12 bytes] || ciphertext+tag)`

The Rust `decrypt_token()` function in `sync-storage-config` must stay byte-for-byte compatible
with the TypeScript `encrypt()`/`decrypt()` in `packages/db/src/encrypt.ts` in the main monorepo,
since both read/write the same DB column.

Encryption key: `TOKEN_ENCRYPTION_KEY` env var — 32 bytes expressed as either 64 hex chars or
44 base64 chars.

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

### Required

| Variable               | Description                                     | Example                            |
|------------------------|-------------------------------------------------|------------------------------------|
| `DATABASE_URL`         | Path to shared SQLite DB                        | `file:/data/anki-cloud.db`         |
| `TOKEN_ENCRYPTION_KEY` | 32-byte AES-256 key (64 hex or 44 base64 chars) | `deadbeef...`                      |
| `GOOGLE_CLIENT_ID`     | Google OAuth2 client ID                         | `123...apps.googleusercontent.com` |
| `GOOGLE_CLIENT_SECRET` | Google OAuth2 client secret                     | `GOCSPX-...`                       |

### Optional (with defaults)

| Variable    | Default         | Description                                     |
|-------------|-----------------|-------------------------------------------------|
| `SYNC_BASE` | `~/.syncserver` | Temp directory for user collections during sync |
| `SYNC_HOST` | `0.0.0.0`       | Bind address                                    |
| `SYNC_PORT` | `8080`          | Bind port                                       |
| `RUST_LOG`  | `anki=info`     | Log level (tracing filter)                      |

`SYNC_*` vars are loaded via `envy::prefixed("SYNC_")` into `SyncServerConfig`.
The others (`DATABASE_URL`, `TOKEN_ENCRYPTION_KEY`, `GOOGLE_CLIENT_*`) are read directly
via `std::env::var()` in `sync-storage-config`.

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
cargo test -p sync-storage-config
cargo test -p sync-storage-backends
cargo test -p sync-storage-api
```

### Docker

```bash
# build image
docker build -t anki-cloud-sync:local .

# run (all required env vars must be set)
docker run \
  -e DATABASE_URL=file:/data/anki-cloud.db \
  -e TOKEN_ENCRYPTION_KEY=<key> \
  -e GOOGLE_CLIENT_ID=<id> \
  -e GOOGLE_CLIENT_SECRET=<secret> \
  -v /path/to/data:/data \
  -p 8080:8080 \
  anki-cloud-sync:local
```

Healthcheck: `anki-sync-server --healthcheck` (exits 0 if running, 1 otherwise).
Used by Docker and docker-compose health checks.

---

## 7. Versioning

This repo uses **Anki version numbers** (e.g. `25.09`), not independent semver.
The sync protocol has a hard dependency on a specific Anki release; the version number
is a meaningful compatibility signal.

A compatibility table in README maps Anki client versions to sync-server image tags.

When Anki releases a new version, the upgrade path is:

1. `./scripts/fork-anki-sync-server.zsh <new-tag>` — re-syncs rslib/ from upstream
2. Verify custom crates still compile against updated rslib
3. Run tests
4. Tag new release matching Anki version (e.g. `v25.10`)

---

## 8. Upstream Upgrade Process

```bash
# re-sync rslib/, ftl/, proto/ from upstream at a given tag
./scripts/fork-anki-sync-server.zsh 25.10

# after syncing:
cargo build --bin anki-sync-server  # verify it compiles
cargo test -p sync-storage-config   # verify custom crates still work
```

**Never manually edit** `rslib/`, `ftl/`, or `proto/`.
If upstream breaks our integration points, fix by adapting the custom crates, not by patching rslib.

---

## 9. Integration with anki-cloud Monorepo

This server is consumed by the main [anki-cloud](https://github.com/danielpmichalski/anki-cloud)
repo as an **external Docker image** in `docker-compose.yml`:

```yaml
sync-server:
  image: ghcr.io/danielpmichalski/anki-cloud-sync:25.09
  environment:
    DATABASE_URL: file:/data/anki-cloud.db
    TOKEN_ENCRYPTION_KEY: ${TOKEN_ENCRYPTION_KEY}
    GOOGLE_CLIENT_ID: ${GOOGLE_CLIENT_ID}
    GOOGLE_CLIENT_SECRET: ${GOOGLE_CLIENT_SECRET}
  volumes:
    - db-data:/data
```

**Dependency contract:**

- Shares the **same SQLite database** as the REST API and DB packages
- Reads from: `users`, `storage_connections`, `users_sync_state` tables
- Writes to: `users_sync_state.sync_key` (upsert on auth)
- Never reads or writes: `api_keys` table
- Schema migrations are owned by `packages/db` in the monorepo — this server is a read/write consumer, not the schema owner

---

## 10. Key Design Principles

1. **Never store deck data.** Collections pass through (temp file during sync), uploaded to user's cloud storage, then deleted locally.
2. **Stateless per request.** No in-memory state that can't be re-derived from DB + OAuth. Safe for restart and horizontal scale.
3. **Custom crates are thin adapters.** They translate between the upstream rslib interfaces and external systems (DB, cloud APIs). Keep them small.
4. **Upstream is upstream.** `rslib/`, `ftl/`, `proto/` are verbatim copies. No hand-edits, ever.
5. **AES-256-GCM encryption format must stay compatible** with `packages/db/src/encrypt.ts` in the monorepo. Both sides read the same DB column.
6. **Conventional commits.** Required for automated changelog and semantic release.
7. **AI Agents: Never auto-commit code.** Inform user that changes are ready; let user handle git commits themselves.

---

## 11. CI/CD

GitHub Actions workflows:

- **`ci.yml`** — runs on every push/PR: `cargo build`, `cargo test -p sync-storage-*`, Docker build
- **`release.yml`** — release-please + conventional commits → auto-bumps version, publishes Docker image to `ghcr.io/danielpmichalski/anki-cloud-sync:<tag>`

Docker image published to: `ghcr.io/danielpmichalski/anki-cloud-sync`

---

*Last updated: extracted from anki-cloud monorepo, 2026-04-19.*

# Upgrade log: Anki 25.09 → 26.09.4

- **Date:** 2026-11-05
- **Branch:** `v26.09.4-alignment`
- **Target tag:** `v26.09.4-r1` (auto-tagged by `.github/workflows/auto-tag.yml` from `.version`, once a PR with a `feat:`/`fix:`/`perf:` commit merges to `main`)
- **Context:** step 0 of the public-readiness plan (anki-cloud repo). No SaaS users existed yet, so no migration concerns.
- **Outcome:** ✅ builds, tests pass, Docker image works, manual sync with Anki Desktop 26.09.4 (macOS) succeeded.

## 1. What changed upstream (25.09 → 26.09.4) that matters to us

Compared pristine `ankitects/anki` trees (shallow clones in a scratch dir, `diff -ruN`).

| Area | Finding |
|---|---|
| Our 5 patched `rslib` source files | `http_server/{mod,user,handlers}.rs`, `sync/main.rs`, `sync/Cargo.toml`: **unchanged upstream** → delta re-applied with zero fuzz/offset |
| `rslib/Cargo.toml`, `collection/tests.rs` | Changed upstream, but in hunks disjoint from ours (new deps; a new test) |
| Sync protocol | `SYNC_VERSION_MIN/MAX` unchanged |
| Collection schema | `SCHEMA_MAX_VERSION` = 18, `storage/upgrades/` identical → no migrations |
| Sync logic | `never_synced()` added; empty new collection no longer stuck on "full sync" (upstream issue #5109). Media `Checksums` case-folding is client-side. Neither touches our trait boundary |
| `proto/` | 10 files changed + new `github.proto`; normal codegen, nothing for us |
| Toolchain | Upstream pins Rust **1.97.1** (we were on 1.95) |
| Workspace deps | `fsrs` 5.1.0→6.6.2, `sha2` 0.10.9→0.11.0, new `permutation`, `same-file`, `locale_config`, plus patch bumps (`ammonia`, `bytes`, `rand`, `tar`), new `linkcheck` rev |

## 2. Procedure (reusable for the next upgrade)

1. **Before touching anything**, capture our rslib delta against *pristine old upstream* (do this in a throwaway clone, **not** by `git fetch --depth=1` inside the project: it adds a shallow boundary to `.git/shallow`):
   ```bash
   S=<scratchpad>
   git clone --depth=1 --branch <old-tag> https://github.com/ankitects/anki.git $S/anki-old
   mkdir -p $S/gen && cd $S/gen && mkdir a b
   ln -s $S/anki-old/rslib a/rslib && ln -s <repo>/rslib b/rslib
   diff -ruN a/rslib b/rslib > $S/rslib-delta-rel.patch    # relative paths → apply with -p1
   ```
   (Absolute paths in the patch headers are dangerous: `patch -p0` may modify the scratch copy instead of the repo.)
2. Clone the new tag the same way and `diff -ruN` old vs new `rslib/`; check whether our 7 delta files changed upstream, plus protocol/schema constants (`SYNC_VERSION_*`, `SCHEMA_*`) and `rust-toolchain.toml`.
3. Dry-run the delta against the *pristine new* tree: `cp -r anki-new/rslib applytest/rslib && cd applytest && patch -p1 --dry-run < rslib-delta-rel.patch`.
4. Run `./scripts/fork-anki-sync-server.zsh <new-tag>` (wipes `rslib/`, `ftl/`, `proto/`, replaces `.version` and `Cargo.lock`).
5. `patch -p1 < rslib-delta-rel.patch` from the repo root. macOS `patch` leaves `*.orig` files, so delete them (`find . -name '*.orig' -not -path './target/*'`).
6. Update the root `Cargo.toml` (not touched by the script) to match upstream workspace deps; bump `rust-version`, `rust-toolchain.toml`, and the Dockerfile `rust:` tag; bump the 3 custom crate versions to `<anki-version>` in semver form (`26.9.4`).
7. Build + test (below), Docker build, manual client sync.
8. Update README / AGENTS.md references and this log.

## 3. Verification performed

| Check | Result |
|---|---|
| `cargo build --bin anki-sync-server` (Rust 1.97.1) | ✅ |
| `cargo test -p sync-platform-api -p sync-storage-backends -p sync-storage-server` | ✅ 3 tests (all Google Drive backend) |
| `cargo test -p anki --lib sync::collection::tests` | ✅ 11/11 (patched upstream integration tests through our auth/resolver wiring; includes new `new_empty_collection_should_not_require_full_sync`) |
| `docker build -t anki-cloud-sync:local .` (`rust:1.97-slim-bookworm`) | ✅ ~57s compile |
| Container `--healthcheck` | ✅ exit 0 |
| `POST /sync/hostKey` good / bad password | ✅ 200 (hkey = `SHA1(user:pass)`) / 403 |
| **Anki Desktop 26.09.4 (macOS) against the container** | ✅ `hostKey`, `meta`, full `upload`, then normal sync (`start` → `applyChanges` → `chunk` → `applyChunk` → `sanityCheck2` → `finish`) all HTTP 200 |

Not run: sidecar API / REST / MCP checks against the new collection schema (belongs to the `anki-cloud` repo step), and the `anki-cloud` side bump (see §5).

## 4. Gotchas hit (so we don't repeat them)

- **Local toolchain too old:** `cargo build` failed with `anki requires rustc 1.97` (had 1.95). Fixed by installing 1.97.1 and adding `rust-toolchain.toml` so it's pinned per repo instead of a per-machine `rustup override`.
- **`protoc` missing:** `anki_proto` build script fails with "Could not find `protoc`" → `brew install protobuf`.
- **Broken rustup:** an interrupted rustup install left `~/.cargo/bin/*` as dangling symlinks (`rustup` binary gone), so the shell reported `command not found: cargo`. Reinstalled rustup via Homebrew; Homebrew's rustup is *keg-only* → put `/opt/homebrew/opt/rustup/bin` on `PATH` and remove the dangling `~/.cargo/bin` shims.
- **No `.dockerignore`:** `COPY . .` would have sent the 16 GB `target/` dir as build context. Added `.dockerignore` (`target/`, `out/`, `.git/`, `.github/`, `.idea/`, `.claude/`, `tmp.md`). Verified nothing in the build reads git or `out/buildhash`.
- **Manual protocol testing with curl:** requests need an `anki-sync` header (`{"v":11,"k":"","c":"x","s":""}`) **and a zstd-compressed body**, otherwise the server answers 400 (`InvalidBoundary` / `Unknown frame descriptor`). That is test-request error, not a server fault.
- **Release automation:** it's `auto-tag.yml` (reads `.version`), *not* release-please. The commit type decides whether a release is cut; `chore:`/`docs:`/`refactor:` don't trigger one.

## 5. Follow-ups

- [ ] Commit (conventional, `feat:` so `auto-tag` cuts `v26.09.4-r1`) and merge to `main`. Don't include untracked `tmp.md`.
- [ ] Verify the published tag/image `ghcr.io/danielpmichalski/anki-cloud-sync:v26.09.4-r1`.
- [ ] `anki-cloud` repo: bump `.sync-server-version` and the `tag =` in all 4 git deps of `sync-platform-cloud/Cargo.toml`; fix any compile errors in `CloudAuthProvider` / `CloudBackendResolver`; `docker build -f Dockerfile.sync .`; run e2e + `scripts/smoke-test.sh`; check sidecar/REST/MCP against schema 18; update CLAUDE.md §4.6.
- [ ] Optional: fix `[profile.dev.package.rsbridge]` warning (`profile package spec rsbridge ... did not match any packages`): leftover from upstream's Cargo.toml, harmless.
- [ ] Optional: `README.md` section "Upgrading to a new Anki release" could point at this log and at the relative-path patch procedure in §2.

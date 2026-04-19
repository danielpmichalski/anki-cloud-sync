# Research: Sync Server Collection Cache & Horizontal Scaling

Date: 2026-04-19

## Background

[ADR-0011](../decisions/0011-use-stateless-horizontally-scalable-sync-server-architecture-with-per-request-db-lookups.md)
describes the sync server as "stateless per request." That is true for **auth and config** — the
shared SQLite DB is queried on every request for OAuth tokens and storage provider. However, it is
**not true for the Anki collection itself.**

## The In-Memory Collection Cache

The sync server (`SimpleServerInner`) holds a `HashMap<String, User>` keyed by hkey. Each `User`
owns `col: Option<Collection>` — the open SQLite handle to `collection.anki2`. Once opened, the
handle stays open across requests. `ensure_col_open()` is a no-op if `col.is_some()`.

```
First request for user:
  ensure_col_open() → col is None → open_collection()
    → fetch_storage_connection()    ← DB lookup
    → exchange_refresh_token()      ← OAuth HTTP
    → backend.fetch(user, dest)     ← GDrive download
    → CollectionBuilder::new().build()
  col = Some(...)   ← cached for all future requests on this instance

Subsequent requests on same instance:
  ensure_col_open() → col is Some → skip download entirely
```

**Collection eviction happens only when:**
1. A new Anki sync begins: `start_new_sync()` → `abort_stateful_sync_if_active()` → `col = None`
2. A sync op fails mid-flight: error handler sets `col = None, sync_state = None`
3. The process restarts

## Implication: The Sync Protocol Already Requires Sticky Sessions

The multi-step Anki sync protocol (hostKey → start → applyGraves → applyChanges → chunk* →
applyChunk* → finish) keeps `sync_state: Option<ServerSyncState>` in memory between HTTP
requests. If the load balancer routes `start` to Instance A and `applyChanges` to Instance B,
Instance B has no `sync_state` and returns 409 Conflict.

**Sticky sessions (session affinity by user) are therefore already required for correctness —
not an optimization.** ADR-0011's claim of "any request can be routed to any instance" is
incorrect for a user mid-sync.

## The Multi-Instance Cache Problem (Discovered During Sidecar Implementation)

The sidecar (REST API → Rust internal HTTP bridge) made this implicit assumption explicit:

### Read Staleness

```
Instance A: user col cached → notes [N1, N2]
Sidecar on A: adds N3, uploads to GDrive, col stays cached → [N1, N2, N3]
Load balancer routes next GET /decks/:id/notes to Instance B
Instance B: col cached from earlier → [N1, N2]  ← stale, N3 missing
```

### Write Data Loss (Critical)

```
Instance A cache: [N1, N2, N3]
Instance B cache: [N1, N2]          ← stale, missed N3
Sidecar on B: adds N4 to stale col → [N1, N2, N4]
Sidecar on B: commits → GDrive now has [N1, N2, N4]
N3 is permanently lost
```

This is the same class of problem as lost-update in distributed systems. Last writer wins, silently.

## Options Considered

### Option A: Sticky Sessions (Session Affinity by User) — Recommended

Route all requests for a given user to the same sync-server instance, keyed on a stable user
identifier (email hash or hkey) at the load balancer.

**Pros:**
- Already required by the Anki sync protocol (no new constraint added)
- Zero code changes to the sync server
- Collection cache works correctly — one instance owns one user's collection at a time
- Sidecar reads skip GDrive download; sidecar writes don't risk losing concurrent writes

**Cons:**
- Uneven load distribution if some users sync far more than others (hot user problem)
- Instance failure requires re-routing affected users, triggering GDrive re-fetch on next instance
- Load balancer must support header-based affinity (hkey in `anki-sync` header, or email in `X-User-Email`)

**Load balancer config (nginx example):**
```nginx
upstream sync_backend {
    hash $http_x_user_email consistent;
    server sync1:8080;
    server sync2:8080;
    server sync3:8080;
}
```
For Traefik, use a `sticky` cookie or custom header hash.

### Option B: No Collection Cache (Fetch+Upload Per Request)

Remove `ensure_col_open()` short-circuit. Every request downloads from GDrive, runs the op,
uploads to GDrive.

**Pros:**
- Truly stateless — any instance can handle any request
- No sticky session requirement

**Cons:**
- 2–10 second latency per request (GDrive roundtrip + OAuth token exchange)
- GDrive API quota burned on every read, including cheap list-decks calls
- Anki sync (50+ sub-requests per sync) would download/upload the collection on every sub-step —
  catastrophic for performance and GDrive quotas

**Verdict:** Not viable.

### Option C: Distributed Write Lock (Redis) + Per-Request Fetch for Sidecar Only

Sidecar handlers acquire a Redis lock keyed `collection-lock:{email}` before opening the
collection. Anki sync handlers do the same. Only one lock holder may read or write at a time.
Sidecar reads skip upload; sidecar writes upload on release.

**Pros:**
- Allows true stateless routing for sidecar requests
- Prevents write conflicts

**Cons:**
- Anki sync is multi-step — lock must be held across all sync sub-steps, requiring Redis-persisted
  sync state (not just in-memory `sync_state`). Significant refactor of rslib internals.
- Lock TTL must be long enough for a full sync (30s–2min). Stale locks block all access.
- Redis becomes a critical dependency — failure blocks all syncs for all users.
- Adds latency per request (lock acquisition roundtrip)

**Verdict:** Correct but overly complex for current scale. Reconsider at 10k+ concurrent users.

### Option D: Single-Instance (No Load Balancer)

Run one sync-server instance. Scale vertically (larger machine, more CPU/RAM).

**Pros:** Simple. Collection cache always coherent.

**Cons:** Vertical scaling has limits. Single point of failure.

**Verdict:** Fine for MVP / self-hosting. Not for hosted platform at scale.

## Recommendation

**Option A (sticky sessions)** is the right call for the current stage:

- It's already required for Anki sync correctness — adding it doesn't constrain the architecture
  further
- Zero code changes required — pure load balancer config
- Collection cache works correctly within an instance
- When an instance fails, affected users are rerouted; their next request triggers a GDrive
  re-fetch (slow once, correct always)

**For sidecar correctness under sticky sessions:**

- Reads (`list_decks`, `get_note`, etc.): use cached collection, no GDrive roundtrip
- Writes (`create_note`, `delete_deck`, etc.): use cached collection, upload to GDrive on completion
- Both return 409 if an Anki sync is in progress for that user (checked via `sync_state.is_some()`)

ADR-0013 formalizes this decision and corrects ADR-0011's incorrect "stateless" claim.

## Open Questions

- **Hot user problem:** A user who syncs constantly will monopolize one instance. Monitor per-user
  request rate; if needed, implement per-user request queuing within an instance.
- **Graceful instance drain:** During rolling deploys, in-flight syncs must complete before an
  instance is terminated. Configure load balancer drain timeout ≥ 120s (max sync duration).
- **Future: Redis-backed sync state:** If we ever want true stateless routing (Option C), the
  first step is persisting `sync_state` to Redis so any instance can continue a sync started on
  another. This is a multi-sprint effort and not warranted until sticky sessions become a
  bottleneck.

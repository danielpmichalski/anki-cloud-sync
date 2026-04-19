# 13. Require sticky sessions (user affinity) for sync-server correctness

Date: 2026-04-19

## Status

Accepted

Partially supersedes [ADR-0011](./0011-use-stateless-horizontally-scalable-sync-server-architecture-with-per-request-db-lookups.md)
(corrects the "stateless per request" claim as it applies to collection handling).

## Context

[ADR-0011](./0011-use-stateless-horizontally-scalable-sync-server-architecture-with-per-request-db-lookups.md)
described the sync server as "stateless per request." This is accurate for **auth and config**:
the shared SQLite DB is queried on every request for hkey validation, OAuth tokens, and storage
provider — no per-user config lives in instance memory.

It is **not accurate for the Anki collection.** The sync server caches each user's open
`collection.anki2` handle (`col: Option<Collection>`) in `SimpleServerInner.users` for the
lifetime of the process. `ensure_col_open()` downloads from GDrive only on the first access per
user per instance; subsequent accesses use the cached handle.

This distinction was made visible when implementing the internal sidecar REST API ([ADR-0010](./0010-rust-sync-server-exposes-internal-sidecar-for-collection-mutations.md)).
Without sticky sessions, two instances could each hold a stale cached collection for the same
user, leading to:

- **Read staleness:** a GET request on Instance B returns data that does not include writes
  committed by Instance A
- **Silent data loss on writes:** Instance B commits a collection that does not include writes
  made by Instance A — those writes are permanently overwritten on GDrive

Additionally, the Anki sync protocol is inherently multi-step (hostKey → start → applyGraves →
applyChanges → chunk* → applyChunk* → finish). Per-step sync state (`ServerSyncState`) is held
in instance memory. Routing `start` to Instance A and `applyChanges` to Instance B results in a
409 Conflict because Instance B has no sync state. **Sticky sessions were therefore already
required for Anki sync protocol correctness** — this ADR makes that requirement explicit and
extends it to sidecar requests.

See [research/sync-server-collection-cache-and-horizontal-scaling.md](../research/sync-server-collection-cache-and-horizontal-scaling.md)
for full analysis of all options considered.

## Decision

The load balancer **must route all requests for a given user to the same sync-server instance.**
Affinity key: a stable per-user identifier, specifically the email hash or the hkey carried in
the `anki-sync` header (for Anki clients) or the `X-User-Email` header (for sidecar requests).

Nginx example:
```nginx
upstream sync_backend {
    hash $http_x_user_email consistent;
    server sync1:8080;
    server sync2:8080;
}
```

No changes to sync-server code are required. Sticky sessions are a load balancer configuration
concern only.

## Consequences

**Easier:**
- Collection cache is always coherent — one instance owns one user's collection at a time
- Sidecar reads are fast (no GDrive roundtrip after first access)
- Sidecar writes are safe (no concurrent writes from different instances for the same user)
- Anki sync protocol correctness is maintained without inter-instance coordination

**More difficult:**
- **Hot user skew:** a very active user monopolizes one instance. Mitigation: monitor
  per-instance load; per-user request queuing within an instance if needed.
- **Instance failure:** affected users are rerouted; their next request triggers a GDrive
  re-fetch on the new instance (slow once, always correct).
- **Rolling deploys:** the load balancer must drain in-flight syncs before terminating an
  instance. Configure drain timeout ≥ 120 seconds (upper bound of a full Anki sync).
- **Load balancer requirement:** must support header-based consistent hashing (Nginx, Traefik,
  HAProxy, and most cloud ALBs support this).

**What has not changed from ADR-0011:**
- Auth and storage config remain stateless per request (DB lookup on every request)
- Horizontal scaling is still achieved by adding instances — sticky sessions do not prevent this
- Each instance still independently fetches OAuth tokens and storage config from the shared DB

## Future Path

If sticky sessions become a bottleneck (hot user problem at scale), the alternative is
Redis-backed sync state so any instance can continue a sync started on another, combined with a
distributed write lock (`collection-lock:{email}`) to prevent concurrent collection writes.
This is a significant refactor of rslib internals and is not warranted until sticky sessions are
demonstrated to be insufficient.

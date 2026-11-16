# Google Drive Sync Latency: What Other Apps Hit and How They Fixed It

Research date: 2026-11-16. Prompted by e2e tests where a Google Drive-backed sync took up to ~7 s.

Question: have other apps/SaaS that sync through Google Drive faced similar latency, and how did they fix it?

**Scope and evidence quality.** This is desk research only (web search). It did not profile our backend. Most sources are
forum threads (mainly rclone) plus one vendor blog; Google's own docs are the only primary sources. Specific numbers
(e.g. "2-3 files/s") are anecdotal, may have changed, and must be re-measured before we rely on them. No solid
write-up was found for how Joplin, Zotero or KeePass deal with Drive latency.

Related: [Google-Drive-API-connectivity.md](Google-Drive-API-connectivity.md) (auth, rate limits, media),
[sync-server-collection-cache-and-horizontal-scaling.md](sync-server-collection-cache-and-horizontal-scaling.md),
issue #29 (global mutex held across Drive I/O), issue #4 (S3-compatible backend).

---

## 1. Findings: what others ran into

| # | Problem | Evidence | Strength |
|---|---------|----------|----------|
| 1 | **File creation is slow regardless of client tuning.** Roughly 2-3 creates/s; 158 small `.md` files took 58 s. Raising `--transfers` / `--checkers` did not help. A secondhand report claims a ~3 writes/s per-account cap that cannot be raised. | [rclone: slow file creation](https://forum.rclone.org/t/google-drive-slow-file-creation/51979), [rclone: many small files](https://forum.rclone.org/t/best-settings-for-many-small-files-to-google-drive/3014), [Globus thread](https://groups.google.com/a/globus.org/g/discuss/c/ja9bZFQtrrc) | Anecdotal; not confirmed in Google docs |
| 2 | **Shared OAuth client ID is throttled globally.** rclone docs: the default client ID has a global queries-per-second limit shared by all its users. A custom client ID helps somewhat; reports are mixed. | [rclone: high error rate / slow upload](https://forum.rclone.org/t/high-error-rate-slow-upload-speed-when-uploading-to-google-drive/13707), rclone drive docs | Documented by rclone; benefit anecdotal |
| 3 | **Listing / lookup by name is slow.** A plain `files.list` took ~2 s in a drive with ~13k files; narrowing the query does not always help. | [Stack Overflow mirror](https://kiwix.lema.org/stackoverflow.com_en_all_2023-11/questions/40530714/google-drive-api-response-is-very-slow) | Single anecdote |
| 4 | **Throttling surfaces as 403 `User rate limit exceeded` / 429.** Google says to retry with exponential backoff; rclone users add `--tpslimit` to stay under the limit rather than trigger errors. | [Drive usage limits](https://developers.google.com/drive/api/guides/limits) | Documented (Google) |
| 5 | **Quota cost differs per method.** Per a vendor blog: edit methods cost 50 units, list methods 100, downloads 200 (matches the usage-limits page for `files.update` / downloads as far as seen). | [Drive usage limits](https://developers.google.com/drive/api/guides/limits), [Fast.io rate-limit guide](https://fast.io/resources/google-drive-connector-rate-limit.md) | Mixed; vendor blog is weak |
| 6 | **Concurrent writes to the same file reduce throughput** (reads unaffected). | One developer comment in search results | Weak; undocumented |

## 2. Fixes others used

| Fix | Source | Applies to us? |
|-----|--------|----------------|
| **Pack many small files into one blob** (zip/archive) before upload. The standard answer to the creation-rate limit. | rclone forum | Yes: we already sync a single `collection.anki2`. Media is the risk area (see issue #31). |
| **Update in place (`files.update`) instead of creating files.** Avoids the slow creation path. | Inferred from #1 | Yes: keep the file ID and update it. |
| **Cache Drive file/folder IDs; never look up by name per request.** | Google performance guide, #3 | Yes: avoid a `files.list` for the `AnkiSync` folder / `collection.anki2` on every sync. |
| **Partial responses (`fields=`).** | [Google: Improve performance](https://developers.google.com/drive/performance) | Yes, cheap. |
| **Use `changes.list` with a stored page token** instead of re-listing. | Google docs, [Fast.io delta-sync guide](https://fast.io/resources/google-drive-delta-sync.md) | Maybe: useful for detecting remote edits without downloading; handle token expiry. |
| **Batch requests.** | [Google: Improve performance](https://developers.google.com/drive/performance) | Limited: removes HTTP overhead, but each call in a batch still counts against quota and it does not fix creation limits. |
| **Own OAuth client ID.** | rclone docs | Yes, and needed for a hosted SaaS anyway. |
| **Client-side throttling + exponential backoff with jitter.** | Google docs, rclone | Already partly done for downloads (see section 3). |
| **Limit concurrent writes per credential to a few; back off on errors.** | Search summary of rclone/Google guidance | Yes. |

## 3. Implications for our backend (hypotheses, not measured)

Current design (per `AGENTS.md`): every sync does `fetch()` (download whole `collection.anki2`) and `commit()`
(resumable upload of the whole file, 256 KB chunks), with folder/file resolution on the way, and the server holds a
global mutex across the Drive I/O (issue #29).

Likely contributors to the ~7 s, to verify with timing logs per Drive call:

1. **Number of API calls per sync** (folder lookup, file lookup, metadata, download, upload init, chunk PUTs).
   Name-based lookups and creates are the expensive kinds per findings 1 and 3.
2. **256 KB upload chunks.** A resumable upload of a multi-MB collection means many sequential round trips. Google
   recommends larger chunks (multiples of 256 KiB); worth testing 4-16 MB or a single-request upload for small files.
   (Chunk-size guidance is from general Drive knowledge, not from the sources above; verify against the Drive docs.)
3. **Whole-file transfer both ways**, even for a small change.
4. **Token refresh on the request path** when the access token is expired (extra round trip to Google's token endpoint).
5. **Serialization**: the global mutex (issue #29) turns per-call latency into queueing for all users.
6. **Shared OAuth client** throttling under test load (finding 2).

## 4. Candidate mitigations, in rough order of expected payoff / effort

1. **Instrument first.** Log duration and count of every Drive call per fetch/commit; find the real breakdown.
2. **Cache Drive file/folder IDs and access tokens** (per user) so sync does no name lookups or refreshes on the hot path.
3. **Tune upload**: larger chunks / simple upload for small collections; `fields=` on all calls.
4. **Keep a local working copy** and make the Drive push asynchronous (write-behind), so the Anki client does not wait
   on Drive. Needs a durability story; see the collection-cache research doc.
5. **Skip unneeded transfers**: compare Drive `md5Checksum`/`modifiedTime` (or use `changes.list`) before downloading;
   skip uploads when nothing changed.
6. **Do not hold the global mutex during Drive I/O** (issue #29).
7. **Use our own OAuth client ID and a conservative `tpslimit`-style limiter** with backoff.
8. **Alternative backends** (issue #4, S3-compatible): object stores have no comparable per-account file-creation cap,
   so this class of latency should not appear there (to be validated, not established by this research).

## 5. Open questions / gaps

- Real per-call timing breakdown of our 7 s (not measured here).
- Whether the ~2-3 creates/s and ~3 writes/s figures still hold; nothing authoritative from Google.
- How Joplin, Zotero, KeePass-family apps and similar handle Drive latency: no reliable source found.
- Whether single-file `files.update` on a multi-MB file has its own latency floor beyond transfer time.

## Sources

- [Google Drive API: Improve performance](https://developers.google.com/drive/performance)
- [Google Drive API: Usage limits](https://developers.google.com/drive/api/guides/limits)
- [rclone forum: Google Drive Slow File Creation](https://forum.rclone.org/t/google-drive-slow-file-creation/51979)
- [rclone forum: Best settings for many small files to Google Drive?](https://forum.rclone.org/t/best-settings-for-many-small-files-to-google-drive/3014)
- [rclone forum: High error rate & slow upload speed](https://forum.rclone.org/t/high-error-rate-slow-upload-speed-when-uploading-to-google-drive/13707)
- [Globus: Google Drive connector transfer slow](https://groups.google.com/a/globus.org/g/discuss/c/ja9bZFQtrrc)
- [Stack Overflow mirror: Google Drive API response is very slow](https://kiwix.lema.org/stackoverflow.com_en_all_2023-11/questions/40530714/google-drive-api-response-is-very-slow)
- [Fast.io: Google Drive Delta Sync API](https://fast.io/resources/google-drive-delta-sync.md) (vendor blog)
- [Fast.io: Google Drive connector rate limits](https://fast.io/resources/google-drive-connector-rate-limit.md) (vendor blog)

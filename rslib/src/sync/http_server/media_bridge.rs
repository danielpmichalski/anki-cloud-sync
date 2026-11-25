// Platform extension (not upstream): keeps the user's media in their cloud storage.
//
// Upstream's `ServerMediaManager` stores media on the local disk. Instead of changing it, this
// bridge runs around its calls (see the hooks in `handlers.rs`):
//   - after an upload it pushes every change since a watermark to the storage backend, then
//     uploads `media.db` and removes the local copies;
//   - before a download it fetches the requested files from the storage backend;
//   - at startup it fetches `media.db` if there is no local copy.
// Backends that don't support media (`supports_media() == false`) are left on the upstream
// disk behaviour, so every hook is a no-op for them.
//
// The watermark (`drive_sync.synced_usn`, a side table inside `media.db`) is the highest media
// USN already pushed. Each push covers everything above it, so a failed push is retried by the
// next call even though upstream has already committed the change.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use sync_platform_api::BackendResolver;
use sync_platform_api::StorageBackend;
use tracing::info;
use tracing::warn;

use crate::prelude::Usn;
use crate::sync::error::HttpResult;
use crate::sync::error::OrHttpErr;
use crate::sync::http_server::user::User;

const MEDIA_DB_FILE: &str = "media.db";
const CHANGES_CHUNK: usize = 1000;
/// Changes pushed per storage call; the watermark moves after each window. The budget is only
/// checked between windows, so one window bounds how far a request can overrun it.
const PUSH_WINDOW: usize = 20;
/// How long one request may spend pushing media before it returns and leaves the rest for the
/// next call. Anki clients time out HTTP requests after about 30 s.
const PUSH_BUDGET: Duration = Duration::from_secs(8);

/// Fetch `media.db` from storage if there is no local copy. Must run before the media database
/// is opened, and fails hard: starting from an empty index would later overwrite the real one.
pub(super) fn fetch_media_index_if_missing(
    resolver: &Arc<dyn BackendResolver>,
    email: &str,
    folder: &Path,
) -> HttpResult<()> {
    let local = folder.join(MEDIA_DB_FILE);
    if local.exists() {
        return Ok(());
    }
    let backend = resolver
        .resolve_for_user(email)
        .or_internal_err("resolve storage backend")?;
    if !backend.supports_media() {
        return Ok(());
    }
    let found = backend
        .media_index_fetch(email, &local)
        .or_internal_err("fetch media index from storage")?;
    info!(found, "media index fetched from storage");
    Ok(())
}

impl User {
    fn media_backend(&self) -> HttpResult<Option<Box<dyn StorageBackend>>> {
        let backend = self
            .backend_resolver
            .resolve_for_user(&self.name)
            .or_internal_err("resolve storage backend")?;
        Ok(backend.supports_media().then_some(backend))
    }

    fn synced_usn(&self) -> HttpResult<i32> {
        let db = &self.media.db.db;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS drive_sync (
               id INTEGER PRIMARY KEY CHECK (id = 0),
               synced_usn INTEGER NOT NULL
             );
             INSERT OR IGNORE INTO drive_sync (id, synced_usn) VALUES (0, 0);",
        )
        .or_internal_err("create drive_sync table")?;
        db.query_row("SELECT synced_usn FROM drive_sync WHERE id = 0", [], |r| {
            r.get(0)
        })
        .or_internal_err("read drive_sync watermark")
    }

    fn set_synced_usn(&self, usn: i32) -> HttpResult<()> {
        self.media
            .db
            .db
            .execute("UPDATE drive_sync SET synced_usn = ?1 WHERE id = 0", [usn])
            .or_internal_err("write drive_sync watermark")?;
        Ok(())
    }

    /// Push media changes made since the watermark to storage, for at most `PUSH_BUDGET`. Call
    /// after upstream has processed an upload. A large backlog is drained across several calls.
    pub(in crate::sync) fn push_media_to_storage(&mut self) -> HttpResult<()> {
        self.push_media_to_storage_within(PUSH_BUDGET)
    }

    /// Like `push_media_to_storage`, but never fails the request: the backlog stays above the
    /// watermark and the next call retries it.
    pub(in crate::sync) fn push_media_to_storage_best_effort(&mut self) {
        if let Err(err) = self.push_media_to_storage() {
            warn!(error = ?err, "background media push failed, will retry");
        }
    }

    /// Pushes in windows of `PUSH_WINDOW` changes and moves the watermark after each window, so
    /// stopping at the budget (or on an error) loses nothing: the rest is picked up next time.
    /// The request is held open for the whole push, so the budget must stay well below the
    /// client's HTTP timeout.
    fn push_media_to_storage_within(&mut self, budget: Duration) -> HttpResult<()> {
        let Some(backend) = self.media_backend()? else {
            return Ok(());
        };

        let started = Instant::now();
        let start = self.synced_usn()?;
        let mut synced = start;
        let mut backlog_left = false;
        'chunks: loop {
            let mut changes = self.media.media_changes_chunk(Usn(synced))?;
            if changes.is_empty() {
                break;
            }
            // The query has no ORDER BY; the watermark below relies on ascending USNs.
            changes.sort_by_key(|c| c.usn.0);
            let chunk_len = changes.len();

            for window in changes.chunks(PUSH_WINDOW) {
                if started.elapsed() >= budget {
                    backlog_left = true;
                    break 'chunks;
                }
                let max_usn = window.last().map(|c| c.usn.0).unwrap_or(synced);

                let mut puts: Vec<(String, PathBuf)> = vec![];
                let mut deletes: Vec<String> = vec![];
                for change in window {
                    if change.sha1.is_empty() {
                        deletes.push(change.fname.clone());
                    } else {
                        let path = self.media.media_folder.join(&change.fname);
                        if path.exists() {
                            puts.push((change.fname.clone(), path));
                        } else {
                            // Already pushed and evicted in an earlier call, or never present.
                            warn!(
                                file = change.fname,
                                "media file missing locally, not pushed"
                            );
                        }
                    }
                }
                backend
                    .media_put_many(&self.name, &puts)
                    .or_internal_err("push media to storage")?;
                backend
                    .media_delete_many(&self.name, &deletes)
                    .or_internal_err("delete media in storage")?;
                for (_, path) in &puts {
                    let _ = std::fs::remove_file(path);
                }

                synced = max_usn;
                self.set_synced_usn(synced)?;
            }
            if chunk_len < CHANGES_CHUNK {
                break;
            }
        }

        if synced != start {
            self.commit_media_index(backend.as_ref())?;
            info!(
                from = start,
                to = synced,
                backlog_left,
                "media pushed to storage"
            );
        }
        Ok(())
    }

    /// Make sure the files a client asked for are on local disk, fetching them from storage.
    /// Returns the names that were fetched, so they can be evicted after the response is built.
    pub(in crate::sync) fn fetch_media_from_storage(
        &mut self,
        files: &[String],
    ) -> HttpResult<Vec<String>> {
        let Some(backend) = self.media_backend()? else {
            return Ok(vec![]);
        };
        // Names come from the client; only trust ones that exist in the media database.
        let entries = self.media.db.get_entries_for_download(files)?;
        let wanted: Vec<String> = entries
            .into_iter()
            .map(|e| e.nfc_filename)
            .filter(|name| !self.media.media_folder.join(name).exists())
            .collect();
        if wanted.is_empty() {
            return Ok(vec![]);
        }
        let missing = backend
            .media_get_many(&self.name, &wanted, &self.media.media_folder)
            .or_internal_err("fetch media from storage")?;
        // Files missing in storage are left for upstream to report ("forget missing file").
        Ok(wanted
            .into_iter()
            .filter(|n| !missing.contains(n))
            .collect())
    }

    /// Remove local copies of media that storage now holds.
    pub(in crate::sync) fn evict_media(&self, names: &[String]) {
        for name in names {
            let _ = std::fs::remove_file(self.media.media_folder.join(name));
        }
    }

    fn commit_media_index(&self, backend: &dyn StorageBackend) -> HttpResult<()> {
        self.media
            .db
            .db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .or_internal_err("checkpoint media db")?;
        backend
            .media_index_commit(&self.name, &self.folder.join(MEDIA_DB_FILE))
            .or_internal_err("commit media index to storage")
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    use super::*;
    use crate::sync::http_server::media_manager::ServerMediaManager;
    use crate::sync::media::zip::zip_files_for_upload;

    /// In-memory stand-in for Drive, shared between "server instances".
    #[derive(Default)]
    pub(in crate::sync::http_server) struct Cloud {
        pub(in crate::sync::http_server) files: Mutex<HashMap<String, Vec<u8>>>,
        index: Mutex<Option<Vec<u8>>>,
        pub(in crate::sync::http_server) fail_puts: AtomicBool,
        put_delay: Mutex<Duration>,
    }

    struct FakeBackend(Arc<Cloud>);

    impl StorageBackend for FakeBackend {
        fn fetch(&self, _: &str, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn commit(&self, _: &str, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn supports_media(&self) -> bool {
            true
        }
        fn media_index_fetch(&self, _: &str, dest: &Path) -> anyhow::Result<bool> {
            match &*self.0.index.lock().unwrap() {
                Some(bytes) => {
                    std::fs::write(dest, bytes)?;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        fn media_index_commit(&self, _: &str, src: &Path) -> anyhow::Result<()> {
            *self.0.index.lock().unwrap() = Some(std::fs::read(src)?);
            Ok(())
        }
        fn media_put_many(&self, _: &str, files: &[(String, PathBuf)]) -> anyhow::Result<()> {
            if self.0.fail_puts.load(Ordering::SeqCst) {
                anyhow::bail!("drive unavailable");
            }
            std::thread::sleep(*self.0.put_delay.lock().unwrap());
            for (name, path) in files {
                self.0
                    .files
                    .lock()
                    .unwrap()
                    .insert(name.clone(), std::fs::read(path)?);
            }
            Ok(())
        }
        fn media_get_many(
            &self,
            _: &str,
            names: &[String],
            dest: &Path,
        ) -> anyhow::Result<Vec<String>> {
            let mut missing = vec![];
            for name in names {
                match self.0.files.lock().unwrap().get(name) {
                    Some(data) => std::fs::write(dest.join(name), data)?,
                    None => missing.push(name.clone()),
                }
            }
            Ok(missing)
        }
        fn media_delete_many(&self, _: &str, names: &[String]) -> anyhow::Result<()> {
            for name in names {
                self.0.files.lock().unwrap().remove(name);
            }
            Ok(())
        }
    }

    struct FakeResolver(Arc<Cloud>);
    impl BackendResolver for FakeResolver {
        fn resolve_for_user(&self, _: &str) -> anyhow::Result<Box<dyn StorageBackend>> {
            Ok(Box::new(FakeBackend(Arc::clone(&self.0))))
        }
    }

    pub(in crate::sync::http_server) fn new_user(cloud: &Arc<Cloud>, folder: &Path) -> User {
        let resolver: Arc<dyn BackendResolver> = Arc::new(FakeResolver(Arc::clone(cloud)));
        fetch_media_index_if_missing(&resolver, "u@example.com", folder).unwrap();
        User {
            name: "u@example.com".into(),
            col: None,
            sync_state: None,
            media: ServerMediaManager::new(folder).unwrap(),
            folder: folder.to_path_buf(),
            backend_resolver: resolver,
        }
    }

    fn upload(user: &mut User, files: Vec<(&str, Option<&str>)>) -> HttpResult<()> {
        let zip = zip_files_for_upload(
            files
                .into_iter()
                .map(|(n, d)| (n.to_string(), d.map(|d| d.as_bytes().to_vec())))
                .collect(),
        )
        .unwrap();
        user.media.process_uploaded_changes(zip)?;
        user.push_media_to_storage()
    }

    pub(in crate::sync::http_server) fn local_media(folder: &Path) -> HashSet<String> {
        std::fs::read_dir(folder.join("media"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn media_round_trips_through_storage_with_no_local_state() {
        let cloud = Arc::new(Cloud::default());
        let dir1 = tempfile::tempdir().unwrap();

        // Upload two files: they land in storage and are evicted locally.
        let mut user = new_user(&cloud, dir1.path());
        upload(
            &mut user,
            vec![("a.png", Some("AAA")), ("b.png", Some("BBB"))],
        )
        .unwrap();
        assert_eq!(cloud.files.lock().unwrap().len(), 2);
        assert!(cloud.index.lock().unwrap().is_some());
        assert!(local_media(dir1.path()).is_empty());
        drop(user);

        // A fresh instance with an empty disk sees the same media and can serve it.
        let dir2 = tempfile::tempdir().unwrap();
        let mut user = new_user(&cloud, dir2.path());
        assert_eq!(user.media.last_usn().unwrap().0, 2);
        let files = vec!["a.png".to_string()];
        let fetched = user.fetch_media_from_storage(&files).unwrap();
        assert_eq!(fetched, files);
        assert!(user.media.zip_files_for_download(files).is_ok());
        user.evict_media(&fetched);
        assert!(local_media(dir2.path()).is_empty());

        // Deleting a file removes it from storage and advances the index.
        upload(&mut user, vec![("b.png", None)]).unwrap();
        assert!(!cloud.files.lock().unwrap().contains_key("b.png"));
        assert!(cloud.files.lock().unwrap().contains_key("a.png"));
    }

    #[test]
    fn failed_push_is_retried_by_the_next_call() {
        let cloud = Arc::new(Cloud::default());
        let dir = tempfile::tempdir().unwrap();
        let mut user = new_user(&cloud, dir.path());

        cloud.fail_puts.store(true, Ordering::SeqCst);
        assert!(upload(&mut user, vec![("a.png", Some("AAA"))]).is_err());
        assert!(cloud.files.lock().unwrap().is_empty());
        // The file stays on local disk, so nothing is lost.
        assert!(local_media(dir.path()).contains("a.png"));

        // Upstream already committed it, so a client retry sees "identical"; the watermark
        // still makes the next push cover it.
        cloud.fail_puts.store(false, Ordering::SeqCst);
        user.push_media_to_storage().unwrap();
        assert_eq!(cloud.files.lock().unwrap().get("a.png").unwrap(), b"AAA");
        assert!(local_media(dir.path()).is_empty());
    }

    #[test]
    fn large_backlog_is_pushed_in_bounded_calls() {
        let cloud = Arc::new(Cloud::default());
        let dir = tempfile::tempdir().unwrap();
        let mut user = new_user(&cloud, dir.path());

        // Stage 300 files locally without pushing, like the backlog behind issue #42.
        *cloud.put_delay.lock().unwrap() = Duration::from_millis(40);
        cloud.fail_puts.store(true, Ordering::SeqCst);
        let names: Vec<String> = (0..300).map(|i| format!("f{i:03}.png")).collect();
        // Clients upload in batches of at most 25 files.
        for batch in names.chunks(25) {
            let files = batch.iter().map(|n| (n.as_str(), Some("x"))).collect();
            assert!(upload(&mut user, files).is_err());
        }
        assert_eq!(local_media(dir.path()).len(), 300);
        assert_eq!(user.synced_usn().unwrap(), 0);
        cloud.fail_puts.store(false, Ordering::SeqCst);

        // 300 files = 6 windows of 40 ms = 240 ms; a 100 ms budget needs several calls, and
        // each call must return shortly after its budget instead of draining everything.
        let mut calls = 0;
        let mut last = 0;
        while user.synced_usn().unwrap() < 300 {
            let t = Instant::now();
            user.push_media_to_storage_within(Duration::from_millis(100))
                .unwrap();
            assert!(
                t.elapsed() < Duration::from_millis(400),
                "call overran its budget"
            );
            let now = user.synced_usn().unwrap();
            assert!(now > last, "every call must make progress");
            last = now;
            calls += 1;
            assert!(calls < 20);
        }
        assert!(calls > 1, "backlog should not fit in a single bounded call");
        assert_eq!(cloud.files.lock().unwrap().len(), 300);
        assert!(local_media(dir.path()).is_empty());
        assert!(cloud.index.lock().unwrap().is_some());
    }

    #[test]
    fn backends_without_media_support_keep_files_on_disk() {
        struct Local;
        impl StorageBackend for Local {
            fn fetch(&self, _: &str, _: &Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn commit(&self, _: &str, _: &Path) -> anyhow::Result<()> {
                Ok(())
            }
        }
        struct LocalResolver;
        impl BackendResolver for LocalResolver {
            fn resolve_for_user(&self, _: &str) -> anyhow::Result<Box<dyn StorageBackend>> {
                Ok(Box::new(Local))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut user = User {
            name: "u@example.com".into(),
            col: None,
            sync_state: None,
            media: ServerMediaManager::new(dir.path()).unwrap(),
            folder: dir.path().to_path_buf(),
            backend_resolver: Arc::new(LocalResolver),
        };
        upload(&mut user, vec![("a.png", Some("AAA"))]).unwrap();
        assert!(local_media(dir.path()).contains("a.png"));
    }
}

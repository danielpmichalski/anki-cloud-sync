// Platform extension (not upstream): media operations for the sidecar (internal API).
//
// These reuse the server-side media code that client syncs use (`ServerMediaManager`,
// `media_bridge`), so a file added here gets a normal USN in `media.db`, is pushed to the user's
// storage and reaches every client on its next media sync. Content rules (type allowlist,
// sizes, SVG sanitising) are the caller's business; this layer only applies Anki's filename
// rules and storage semantics.

use crate::media::files::add_hash_suffix_to_file_stem;
use crate::media::files::normalize_filename;
use crate::media::files::sha1_of_data;
use crate::sync::error::HttpResult;
use crate::sync::error::OrHttpErr;
use crate::sync::http_server::user::User;
use crate::sync::media::zip::zip_files_for_upload;

pub struct SidecarMediaEntry {
    pub name: String,
    pub size: u64,
}

pub struct SidecarStoredMedia {
    /// The name the file is stored under. Can differ from the requested one: Anki normalises
    /// and lowercases names, and appends a hash when the name is taken by different content.
    pub name: String,
    /// False when identical content was already stored under that name.
    pub added: bool,
}

impl User {
    /// Add a file the way Anki's own `add_file` does: normalised and lowercased name, no write
    /// when identical content exists, a hash suffix when the name is taken by other content.
    /// Pushes to storage before returning and fails if that fails. A failed push leaves the
    /// file above the watermark, so a retry (or the next client media call) finishes it.
    pub(super) fn sidecar_add_media(
        &mut self,
        desired_name: &str,
        data: Vec<u8>,
    ) -> HttpResult<SidecarStoredMedia> {
        if data.is_empty() {
            None.or_bad_request("empty file")?;
        }
        let lowered = normalize_filename(desired_name).to_lowercase();
        let mut name = normalize_filename(&lowered).into_owned();
        if name.is_empty() {
            None.or_bad_request("empty file name")?;
        }

        let sha1 = sha1_of_data(&data);
        let mut added = true;
        let mut existing = self
            .media
            .db
            .get_nonempty_entry(&name)
            .or_internal_err("read media entry")?;
        if matches!(&existing, Some(e) if e.sha1 == sha1) {
            added = false;
        } else if existing.is_some() {
            name = add_hash_suffix_to_file_stem(&name, &sha1);
            existing = self
                .media
                .db
                .get_nonempty_entry(&name)
                .or_internal_err("read media entry")?;
            added = !matches!(&existing, Some(e) if e.sha1 == sha1);
        }

        if added {
            let zip = zip_files_for_upload(vec![(name.clone(), Some(data))])
                .or_internal_err("zip media file")?;
            self.media.process_uploaded_changes(zip)?;
        }
        // Also when nothing was added: finishes a push that failed on an earlier attempt.
        self.push_media_to_storage()?;
        Ok(SidecarStoredMedia { name, added })
    }

    /// All stored files (deleted ones excluded), sorted by name. Reads `media.db` only.
    pub(super) fn sidecar_list_media(&self) -> HttpResult<Vec<SidecarMediaEntry>> {
        let mut stmt = self
            .media
            .db
            .db
            .prepare("SELECT fname, size FROM media WHERE size > 0 ORDER BY fname")
            .or_internal_err("list media")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SidecarMediaEntry {
                    name: row.get(0)?,
                    size: row.get::<_, i64>(1)? as u64,
                })
            })
            .or_internal_err("list media")?;
        rows.collect::<Result<Vec<_>, _>>()
            .or_internal_err("list media")
    }

    /// The file's bytes, fetched from storage on demand and evicted locally afterwards.
    /// `None` when the name is not in the media index (or storage lost the file).
    pub(super) fn sidecar_get_media(&mut self, name: &str) -> HttpResult<Option<Vec<u8>>> {
        // Only names in the index are trusted: this also keeps `name` from escaping the folder.
        if self
            .media
            .db
            .get_nonempty_entry(name)
            .or_internal_err("read media entry")?
            .is_none()
        {
            return Ok(None);
        }
        let fetched = self.fetch_media_from_storage(&[name.to_string()])?;
        let read = std::fs::read(self.media.media_folder.join(name));
        self.evict_media(&fetched);
        match read {
            Ok(data) => Ok(Some(data)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).or_internal_err("read media file"),
        }
    }

    /// Delete a file: tombstone in `media.db` (clients delete it on their next sync), then remove
    /// it from storage. Returns false when there was no such file.
    pub(super) fn sidecar_delete_media(&mut self, name: &str) -> HttpResult<bool> {
        if self
            .media
            .db
            .get_nonempty_entry(name)
            .or_internal_err("read media entry")?
            .is_none()
        {
            return Ok(false);
        }
        let zip = zip_files_for_upload(vec![(name.to_string(), None)])
            .or_internal_err("zip media deletion")?;
        self.media.process_uploaded_changes(zip)?;
        self.push_media_to_storage()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use crate::sync::http_server::media_bridge::tests::local_media;
    use crate::sync::http_server::media_bridge::tests::new_user;
    use crate::sync::http_server::media_bridge::tests::Cloud;

    #[test]
    fn add_list_get_delete_round_trip_through_storage() {
        let cloud = Arc::new(Cloud::default());
        let dir = tempfile::tempdir().unwrap();
        let mut user = new_user(&cloud, dir.path());

        // Names follow Anki's rules: lowercased, forbidden characters removed.
        let stored = user.sidecar_add_media("Cat?.PNG", b"AAA".to_vec()).unwrap();
        assert_eq!(stored.name, "cat.png");
        assert!(stored.added);
        assert_eq!(cloud.files.lock().unwrap().get("cat.png").unwrap(), b"AAA");
        assert!(local_media(dir.path()).is_empty());

        let list = user.sidecar_list_media().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].name.as_str(), list[0].size), ("cat.png", 3));

        // Served from storage and evicted again.
        assert_eq!(user.sidecar_get_media("cat.png").unwrap().unwrap(), b"AAA");
        assert!(local_media(dir.path()).is_empty());
        assert!(user.sidecar_get_media("nope.png").unwrap().is_none());
        assert!(user.sidecar_get_media("../collection.anki2").unwrap().is_none());

        assert!(user.sidecar_delete_media("cat.png").unwrap());
        assert!(!user.sidecar_delete_media("cat.png").unwrap());
        assert!(user.sidecar_list_media().unwrap().is_empty());
        assert!(cloud.files.lock().unwrap().is_empty());
    }

    #[test]
    fn same_content_is_not_added_twice_and_other_content_gets_a_hash_suffix() {
        let cloud = Arc::new(Cloud::default());
        let dir = tempfile::tempdir().unwrap();
        let mut user = new_user(&cloud, dir.path());

        assert!(user.sidecar_add_media("a.png", b"one".to_vec()).unwrap().added);
        let usn = user.media.last_usn().unwrap();
        let again = user.sidecar_add_media("A.png", b"one".to_vec()).unwrap();
        assert_eq!(again.name, "a.png");
        assert!(!again.added);
        assert_eq!(user.media.last_usn().unwrap(), usn, "no new USN for identical content");

        let other = user.sidecar_add_media("a.png", b"two".to_vec()).unwrap();
        assert!(other.added);
        assert_ne!(other.name, "a.png");
        assert!(other.name.starts_with("a-") && other.name.ends_with(".png"));
        assert_eq!(user.sidecar_list_media().unwrap().len(), 2);
        // The original is untouched.
        assert_eq!(user.sidecar_get_media("a.png").unwrap().unwrap(), b"one");
    }

    #[test]
    fn rejects_empty_input() {
        let cloud = Arc::new(Cloud::default());
        let dir = tempfile::tempdir().unwrap();
        let mut user = new_user(&cloud, dir.path());
        assert!(user.sidecar_add_media("a.png", vec![]).is_err());
        assert!(user.sidecar_add_media("??", b"x".to_vec()).is_err());
    }

    #[test]
    fn failed_push_is_an_error_and_a_retry_finishes_it() {
        let cloud = Arc::new(Cloud::default());
        let dir = tempfile::tempdir().unwrap();
        let mut user = new_user(&cloud, dir.path());

        cloud.fail_puts.store(true, Ordering::SeqCst);
        assert!(user.sidecar_add_media("a.png", b"AAA".to_vec()).is_err());
        assert!(cloud.files.lock().unwrap().is_empty());
        // The failed add loses nothing: the bytes stay on local disk until a push succeeds.
        assert!(local_media(dir.path()).contains("a.png"));

        // Retrying the same add stores it, though the index already knows the file.
        cloud.fail_puts.store(false, Ordering::SeqCst);
        let stored = user.sidecar_add_media("a.png", b"AAA".to_vec()).unwrap();
        assert_eq!(stored.name, "a.png");
        assert!(!stored.added);
        assert_eq!(cloud.files.lock().unwrap().get("a.png").unwrap(), b"AAA");
        assert!(local_media(dir.path()).is_empty());
    }
}

use std::path::{Path, PathBuf};

use anyhow::Result;

pub trait StorageBackend: Send + Sync {
    /// Download the user's collection to `dest` before sync begins.
    fn fetch(&self, user: &str, dest: &Path) -> Result<()>;

    /// Upload the user's collection from `src` after sync completes.
    fn commit(&self, user: &str, src: &Path) -> Result<()>;

    // ---- Media (optional) ---------------------------------------------------------------
    //
    // Backends that return `true` from `supports_media` keep the user's media files and
    // media index (`media.db`) in cloud storage. Every other backend leaves media on the
    // sync server's local disk, which is the upstream behaviour. The defaults below make
    // adding media support non-breaking for existing implementations.

    /// Whether this backend stores media files in cloud storage.
    fn supports_media(&self) -> bool {
        false
    }

    /// Download the media index to `dest`. Returns `false` if none exists in storage yet.
    fn media_index_fetch(&self, _user: &str, _dest: &Path) -> Result<bool> {
        Ok(false)
    }

    /// Upload the media index from `src`.
    fn media_index_commit(&self, _user: &str, _src: &Path) -> Result<()> {
        Ok(())
    }

    /// Upload media files, given as `(media filename, local path)` pairs. An existing file
    /// with the same name is replaced.
    fn media_put_many(&self, _user: &str, _files: &[(String, PathBuf)]) -> Result<()> {
        Ok(())
    }

    /// Download media files into `dest_dir`, one file per name. Returns the names that do
    /// not exist in storage.
    fn media_get_many(&self, _user: &str, names: &[String], _dest_dir: &Path) -> Result<Vec<String>> {
        Ok(names.to_vec())
    }

    /// Delete media files by name. Names that do not exist are ignored.
    fn media_delete_many(&self, _user: &str, _names: &[String]) -> Result<()> {
        Ok(())
    }
}

/// Resolves a ready-to-use [`StorageBackend`] for a given username.
/// Implementations handle provider lookup, token exchange, and factory selection.
pub trait BackendResolver: Send + Sync {
    fn resolve_for_user(&self, username: &str) -> Result<Box<dyn StorageBackend>>;
}

/// Authenticates sync users and maps session keys to identities.
/// Implementations handle both credential verification and hkey persistence.
pub trait AuthProvider: Send + Sync {
    /// Validate credentials. Returns `(hkey, email)` on success.
    fn authenticate(&self, username: &str, password: &str) -> Result<(String, String)>;

    /// Reverse-lookup: `hkey` → `email`. Called once per authenticated request.
    fn lookup_by_hkey(&self, hkey: &str) -> Result<String>;
}

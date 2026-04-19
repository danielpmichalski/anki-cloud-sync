use std::path::Path;

use anyhow::Result;

pub trait StorageBackend: Send + Sync {
    /// Download the user's collection to `dest` before sync begins.
    fn fetch(&self, user: &str, dest: &Path) -> Result<()>;

    /// Upload the user's collection from `src` after sync completes.
    fn commit(&self, user: &str, src: &Path) -> Result<()>;
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

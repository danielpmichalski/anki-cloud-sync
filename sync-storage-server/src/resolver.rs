use anyhow::Result;
use sync_storage_api::{BackendResolver, StorageBackend};
use sync_storage_backends::StorageBackendFactory;

/// No-op: collection lives on local filesystem. Used for dev / Standalone mode.
pub struct StandaloneBackendResolver;

impl BackendResolver for StandaloneBackendResolver {
    fn resolve_for_user(&self, _username: &str) -> Result<Box<dyn StorageBackend>> {
        StorageBackendFactory::create("local", "", "")
    }
}

/// Looks up storage config from DB, exchanges OAuth token, creates provider-specific backend.
/// This is the single canonical location of the backend-resolution logic (previously duplicated 4×).
pub struct CloudBackendResolver;

impl BackendResolver for CloudBackendResolver {
    fn resolve_for_user(&self, username: &str) -> Result<Box<dyn StorageBackend>> {
        use sync_storage_config as db;
        let (provider, refresh_token, folder_path) = db::fetch_storage_connection(username)?;
        let access_token = if provider == "local" {
            String::new()
        } else {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(db::exchange_refresh_token(&refresh_token))
            })?
        };
        StorageBackendFactory::create(&provider, &access_token, &folder_path)
    }
}

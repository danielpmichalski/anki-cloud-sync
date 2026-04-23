use anyhow::Result;
use sync_platform_api::{BackendResolver, StorageBackend};
use sync_storage_backends::StorageBackendFactory;

/// No-op: collection lives on local filesystem. Used for dev / Standalone mode.
pub struct StandaloneBackendResolver;

impl BackendResolver for StandaloneBackendResolver {
    fn resolve_for_user(&self, _username: &str) -> Result<Box<dyn StorageBackend>> {
        StorageBackendFactory::create("local", "", "")
    }
}

mod auth;
mod handlers;
mod resolver;
mod sidecar;

use std::sync::Arc;

use anki::sync::http_server::SimpleServer;
use anki::sync::http_server::SyncServerConfig;
use anki::error;
use snafu::ResultExt;
use snafu::Whatever;
use sync_storage_api::AuthProvider;
use sync_storage_api::BackendResolver;

pub use auth::{CloudAuthProvider, StandaloneAuthProvider};
pub use resolver::{CloudBackendResolver, StandaloneBackendResolver};
pub use sidecar::InternalServer;

pub enum SyncMode {
    Standalone,
    Cloud,
}

pub fn make_providers(
    mode: SyncMode,
) -> error::Result<(Arc<dyn AuthProvider>, Arc<dyn BackendResolver>), Whatever> {
    Ok(match mode {
        SyncMode::Standalone => (
            Arc::new(
                StandaloneAuthProvider::from_env()
                    .whatever_context("load SYNC_USER* env vars")?,
            ),
            Arc::new(StandaloneBackendResolver),
        ),
        SyncMode::Cloud => (Arc::new(CloudAuthProvider), Arc::new(CloudBackendResolver)),
    })
}

/// Read `SYNC_MODE` env var (default: standalone).
pub fn mode_from_env() -> SyncMode {
    match std::env::var("SYNC_MODE").as_deref() {
        Ok("cloud") => SyncMode::Cloud,
        _ => SyncMode::Standalone,
    }
}

#[snafu::report]
#[tokio::main]
pub async fn run() -> error::Result<(), Whatever> {
    let config = envy::prefixed("SYNC_")
        .from_env::<SyncServerConfig>()
        .whatever_context("reading SYNC_* env vars")?;

    let mode = mode_from_env();
    let (auth, resolver) = make_providers(mode)?;
    let server = Arc::new(
        SimpleServer::new(&config.base_folder, auth, resolver)
            .whatever_context("create server")?,
    );

    if let Some(token) = config.internal_token.clone() {
        let sidecar = InternalServer::new(Arc::clone(&server), token);
        let port = config.internal_port;
        let host = config.internal_host;
        tokio::spawn(async move { sidecar.run(host, port).await });
    }

    let (_addr, server_fut) = SimpleServer::make_server(config, server)
        .await
        .whatever_context("start server")?;
    server_fut.await.whatever_context("await server")?;
    Ok(())
}

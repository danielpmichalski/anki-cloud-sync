// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

mod handlers;
mod logging;
mod media_manager;
mod routes;
mod user;

use std::collections::HashMap;
use std::future::Future;
use std::future::IntoFuture;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use anki_io::create_dir_all;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::Router;
use axum_client_ip::ClientIpSource;
use snafu::ResultExt;
use snafu::Whatever;
use sync_platform_api::AuthProvider;
use sync_platform_api::BackendResolver;
use tokio::net::TcpListener;
use tracing::Span;

use crate::error;
use crate::media::files::sha1_of_data;
use crate::sync::error::HttpResult;
use crate::sync::error::OrHttpErr;
use crate::sync::http_server::logging::with_logging_layer;
use crate::sync::http_server::media_manager::ServerMediaManager;
use crate::sync::http_server::routes::collection_sync_router;
use crate::sync::http_server::routes::health_check_handler;
use crate::sync::http_server::routes::media_sync_router;
use crate::sync::http_server::user::User;
use crate::sync::login::HostKeyRequest;
use crate::sync::login::HostKeyResponse;
use crate::sync::request::SyncRequest;
use crate::sync::request::MAXIMUM_SYNC_PAYLOAD_BYTES;
use crate::sync::response::SyncResponse;

pub struct SimpleServer {
    state: Mutex<SimpleServerInner>,
    base_folder: PathBuf,
    auth: Arc<dyn AuthProvider>,
    backend_resolver: Arc<dyn BackendResolver>,
}

pub struct SimpleServerInner {
    /// hkey->user
    users: HashMap<String, User>,
}

#[derive(serde::Deserialize, Debug)]
pub struct SyncServerConfig {
    #[serde(default = "default_host")]
    pub host: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_base", rename = "base")]
    pub base_folder: PathBuf,
    #[serde(default = "default_ip_header")]
    pub ip_header: ClientIpSource,
    #[serde(default = "default_internal_port")]
    pub internal_port: u16,
    #[serde(default = "default_internal_host")]
    pub internal_host: IpAddr,
    #[serde(default)]
    pub internal_token: Option<String>,
}

fn default_internal_port() -> u16 {
    8081
}

fn default_internal_host() -> IpAddr {
    "127.0.0.1".parse().unwrap()
}

fn default_host() -> IpAddr {
    "0.0.0.0".parse().unwrap()
}

fn default_port() -> u16 {
    8080
}

fn default_base() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| panic!("Unable to determine home folder; please set SYNC_BASE"))
        .join(".syncserver")
}

pub fn default_ip_header() -> ClientIpSource {
    ClientIpSource::ConnectInfo
}

/// Derives a session key from a user+password string (SHA-1 hex).
pub fn derive_hkey(user_and_pass: &str) -> String {
    hex::encode(sha1_of_data(user_and_pass.as_bytes()))
}

impl SimpleServerInner {
    fn ensure_user(
        &mut self,
        hkey: &str,
        email: &str,
        base_folder: &Path,
        backend_resolver: Arc<dyn BackendResolver>,
    ) -> HttpResult<()> {
        if self.users.contains_key(hkey) {
            return Ok(());
        }
        // Evict any stale entry for the same user (e.g., after password reset with a new hkey).
        self.users.retain(|_, u| u.name != email);
        let folder = base_folder.join(email);
        create_dir_all(&folder).or_internal_err("create user folder")?;
        let media = ServerMediaManager::new(&folder).or_internal_err("open media")?;
        self.users.insert(
            hkey.to_string(),
            User {
                name: email.to_string(),

                col: None,
                sync_state: None,
                media,
                folder,
                backend_resolver,
            },
        );
        Ok(())
    }

    /// Find or create a User entry by email for sidecar (internal API) requests.
    pub(super) fn get_or_create_sidecar_user<'a>(
        &'a mut self,
        email: &str,
        base_folder: &Path,
        backend_resolver: Arc<dyn BackendResolver>,
    ) -> HttpResult<&'a mut User> {
        let existing_hkey = self
            .users
            .iter()
            .find(|(_, u)| u.name == email)
            .map(|(k, _)| k.clone());

        if let Some(hkey) = existing_hkey {
            return Ok(self.users.get_mut(&hkey).unwrap());
        }

        let sidecar_hkey = format!("sidecar:{email}");
        let folder = base_folder.join(email);
        create_dir_all(&folder).or_internal_err("create user folder")?;
        let media = ServerMediaManager::new(&folder).or_internal_err("open media")?;
        self.users.insert(
            sidecar_hkey.clone(),
            User {
                name: email.to_string(),

                col: None,
                sync_state: None,
                media,
                folder,
                backend_resolver,
            },
        );
        Ok(self.users.get_mut(&sidecar_hkey).unwrap())
    }
}

/// Opaque handle exposing collection operations to sidecar (internal API) code.
/// Prevents external crates from holding `&mut User` directly.
pub struct SidecarUserHandle<'a> {
    user: &'a mut User,
}

impl<'a> SidecarUserHandle<'a> {
    pub fn with_col<F, T>(&mut self, op: F) -> HttpResult<T>
    where
        F: FnOnce(&mut crate::collection::Collection) -> HttpResult<T>,
    {
        self.user.with_col(op)
    }

    pub fn with_col_and_commit<F, R>(&mut self, op: F) -> HttpResult<R>
    where
        F: FnOnce(&mut crate::collection::Collection) -> HttpResult<R>,
    {
        self.user.with_col_and_commit(op)
    }
}

impl SimpleServer {
    pub(in crate::sync) async fn with_authenticated_user<F, I, O>(
        &self,
        req: SyncRequest<I>,
        op: F,
    ) -> HttpResult<O>
    where
        F: FnOnce(&mut User, SyncRequest<I>) -> HttpResult<O>,
    {
        let email = self.auth.lookup_by_hkey(&req.sync_key).or_forbidden("invalid hkey")?;

        let mut state = self.state.lock().unwrap();
        state.ensure_user(
            &req.sync_key,
            &email,
            &self.base_folder,
            Arc::clone(&self.backend_resolver),
        )?;
        let user = state
            .users
            .get_mut(&req.sync_key)
            .or_forbidden("invalid hkey")?;
        Span::current().record("uid", &user.name);
        Span::current().record("client", &req.client_version);
        Span::current().record("session", &req.session_key);
        op(user, req)
    }

    pub(in crate::sync) fn get_host_key(
        &self,
        request: HostKeyRequest,
    ) -> HttpResult<SyncResponse<HostKeyResponse>> {
        let (hkey, email) = self
            .auth
            .authenticate(&request.username, &request.password)
            .or_forbidden("invalid user/pass")?;

        let mut state = self.state.lock().unwrap();
        state.ensure_user(
            &hkey,
            &email,
            &self.base_folder,
            Arc::clone(&self.backend_resolver),
        )?;
        SyncResponse::try_from_obj(HostKeyResponse { key: hkey })
    }

    /// Run `op` with a sidecar (internal API) user handle for `email`.
    /// Returns 409 if a sync is in progress for that user.
    pub fn with_sidecar_user<F, R>(&self, email: &str, op: F) -> HttpResult<R>
    where
        F: FnOnce(&mut SidecarUserHandle<'_>) -> HttpResult<R>,
    {
        let mut state = self.state.lock().unwrap();
        let user = state.get_or_create_sidecar_user(
            email,
            &self.base_folder,
            Arc::clone(&self.backend_resolver),
        )?;
        if user.sync_state.is_some() {
            return None.or_conflict("sync in progress, try again later")?;
        }
        op(&mut SidecarUserHandle { user })
    }

    pub fn base_folder(&self) -> &Path {
        &self.base_folder
    }

    pub fn is_running() -> bool {
        let config = envy::prefixed("SYNC_")
            .from_env::<SyncServerConfig>()
            .unwrap();
        std::net::TcpStream::connect(format!("{}:{}", config.host, config.port)).is_ok()
    }

    pub fn new(
        base_folder: &Path,
        auth: Arc<dyn AuthProvider>,
        backend_resolver: Arc<dyn BackendResolver>,
    ) -> error::Result<Self, Whatever> {
        Ok(SimpleServer {
            state: Mutex::new(SimpleServerInner {
                users: Default::default(),
            }),
            base_folder: base_folder.to_path_buf(),
            auth,
            backend_resolver,
        })
    }

    pub async fn make_server(
        config: SyncServerConfig,
        server: Arc<SimpleServer>,
    ) -> error::Result<(SocketAddr, ServerFuture), Whatever> {
        let address = &format!("{}:{}", config.host, config.port);
        let listener = TcpListener::bind(address)
            .await
            .with_whatever_context(|_| format!("couldn't bind to {address}"))?;
        let addr = listener.local_addr().unwrap();
        let router = with_logging_layer(
            Router::new()
                .nest("/sync", collection_sync_router())
                .nest("/msync", media_sync_router())
                .route("/health", get(health_check_handler))
                .with_state(server)
                .layer(DefaultBodyLimit::max(*MAXIMUM_SYNC_PAYLOAD_BYTES))
                .layer(config.ip_header.into_extension()),
        );
        let future = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .into_future();
        tracing::info!(%addr, "listening");
        Ok((addr, Box::pin(future)))
    }
}

pub type ServerFuture = Pin<Box<dyn Future<Output = error::Result<(), std::io::Error>> + Send>>;

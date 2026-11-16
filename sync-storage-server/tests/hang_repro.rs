//! Regression test for anki-cloud-sync issue #29: the server used to wedge when several
//! requests arrived while one held the global state lock and blocked on network I/O
//! (all runtime workers ended up parked on the lock, so nothing drove the I/O reactor).
//!
//! The "Drive" is a local HTTP server on its own runtime that answers after a delay.
//! The backend calls it with `block_in_place(|| Handle::current().block_on(..))`,
//! exactly like `GoogleDriveBackend`. The server under test runs on a runtime with
//! only 2 workers so the starvation shows up with a handful of requests.
//! The client side runs on a SEPARATE runtime so its timeout still fires if the
//! server runtime is wedged.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anki::sync::http_server::default_ip_header;
use anki::sync::http_server::SimpleServer;
use anki::sync::http_server::SyncServerConfig;
use sync_platform_api::AuthProvider;
use sync_platform_api::BackendResolver;
use sync_platform_api::StorageBackend;
use sync_storage_server::InternalServer;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const DRIVE_DELAY: Duration = Duration::from_millis(500);
const BURST: usize = 8;

struct NoAuth;
impl AuthProvider for NoAuth {
    fn authenticate(&self, _: &str, _: &str) -> anyhow::Result<(String, String)> {
        anyhow::bail!("unused")
    }
    fn lookup_by_hkey(&self, _: &str) -> anyhow::Result<String> {
        anyhow::bail!("unused")
    }
}

struct SlowDrive {
    url: String,
}
impl SlowDrive {
    fn call(&self) -> anyhow::Result<()> {
        // same shape as GoogleDriveBackend::fetch/commit
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                reqwest::get(&self.url).await?.bytes().await?;
                Ok(())
            })
        })
    }
}
impl StorageBackend for SlowDrive {
    fn fetch(&self, _: &str, _: &Path) -> anyhow::Result<()> {
        self.call()
    }
    fn commit(&self, _: &str, _: &Path) -> anyhow::Result<()> {
        self.call()
    }
}

struct SlowResolver {
    url: String,
}
impl BackendResolver for SlowResolver {
    fn resolve_for_user(&self, _: &str) -> anyhow::Result<Box<dyn StorageBackend>> {
        Ok(Box::new(SlowDrive { url: self.url.clone() }))
    }
}

/// Fake Drive on its own runtime/thread: replies "ok" after DRIVE_DELAY.
fn start_fake_drive() -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap()).unwrap();
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf).await;
                    tokio::time::sleep(DRIVE_DELAY).await;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                        .await;
                });
            }
        });
    });
    rx.recv().unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn burst_of_sidecar_writes_does_not_wedge_server() {
    let drive = start_fake_drive();
    let tmp = tempfile::tempdir().unwrap();
    let (sync_port, side_port) = (free_port(), free_port());

    // Control: REPRO_WORKERS=32 should finish in ~BURST*2*DRIVE_DELAY if the hang is worker starvation.
    let workers: usize = std::env::var("REPRO_WORKERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();

    let server = Arc::new(
        SimpleServer::new(
            tmp.path(),
            Arc::new(NoAuth),
            Arc::new(SlowResolver { url: format!("http://{drive}/") }),
        )
        .unwrap(),
    );
    let config = SyncServerConfig {
        host: "127.0.0.1".parse().unwrap(),
        port: sync_port,
        base_folder: tmp.path().to_path_buf(),
        ip_header: default_ip_header(),
        internal_port: side_port,
        internal_host: "127.0.0.1".parse().unwrap(),
        internal_token: Some("tok".into()),
    };
    server_rt.block_on(async {
        let (_addr, fut) = SimpleServer::make_server(config, Arc::clone(&server)).await.unwrap();
        tokio::spawn(fut);
        let sidecar = InternalServer::new(Arc::clone(&server), "tok".into());
        tokio::spawn(async move { sidecar.run("127.0.0.1".parse().unwrap(), side_port).await });
    });
    std::thread::sleep(Duration::from_millis(300));

    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let started = Instant::now();
    let (ok, health_ok) = client_rt.block_on(async {
        let client = reqwest::Client::new();
        let mut tasks = vec![];
        for i in 0..BURST {
            let c = client.clone();
            tasks.push(tokio::spawn(async move {
                c.post(format!("http://127.0.0.1:{side_port}/internal/v1/decks"))
                    .header("X-Internal-Token", "tok")
                    .header("X-User-Email", "alice@example.com")
                    .json(&serde_json::json!({ "name": format!("deck{i}") }))
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
            }));
        }
        // /health does not touch the lock; it only needs a free runtime worker.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let health = tokio::time::timeout(
            Duration::from_secs(3),
            client.get(format!("http://127.0.0.1:{sync_port}/health")).send(),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);

        let all = tokio::time::timeout(Duration::from_secs(30), async {
            let mut codes = vec![];
            for t in tasks {
                codes.push(t.await.unwrap().ok());
            }
            codes
        })
        .await;
        (all, health)
    });
    let elapsed = started.elapsed();
    eprintln!("elapsed={elapsed:?} health_responsive_during_burst={health_ok} results={ok:?}");
    server_rt.shutdown_background();

    let codes = ok.expect("DEADLOCK REPRODUCED: burst did not finish within 30s");
    assert!(codes.iter().all(|c| *c == Some(201)), "unexpected codes: {codes:?}");
    assert!(health_ok, "/health unresponsive while sidecar burst in flight");
}

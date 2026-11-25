//! End-to-end test of the sidecar media endpoints (`/internal/v1/media*`) over HTTP, on the
//! local backend (no cloud storage): add, list, get, metadata, delete, and the content rules.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anki::sync::http_server::default_ip_header;
use anki::sync::http_server::SimpleServer;
use anki::sync::http_server::SyncServerConfig;
use serde_json::Value;
use sync_platform_api::AuthProvider;
use sync_platform_api::BackendResolver;
use sync_platform_api::StorageBackend;
use sync_storage_server::InternalServer;

struct NoAuth;
impl AuthProvider for NoAuth {
    fn authenticate(&self, _: &str, _: &str) -> anyhow::Result<(String, String)> {
        anyhow::bail!("unused")
    }
    fn lookup_by_hkey(&self, _: &str) -> anyhow::Result<String> {
        anyhow::bail!("unused")
    }
}

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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

struct Api {
    c: reqwest::Client,
    base: String,
}
impl Api {
    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.c
            .request(m, format!("{}{path}", self.base))
            .header("X-Internal-Token", "tok")
            .header("X-User-Email", "alice@example.com")
    }
    async fn add(&self, name: &str, body: Vec<u8>) -> (u16, Value) {
        let r = self
            .req(reqwest::Method::POST, &format!("/internal/v1/media?name={name}"))
            .body(body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    async fn get_json(&self, path: &str) -> (u16, Value) {
        let r = self.req(reqwest::Method::GET, path).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
}

#[test]
fn media_add_list_get_metadata_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let (sync_port, side_port) = (free_port(), free_port());
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let server = Arc::new(SimpleServer::new(tmp.path(), Arc::new(NoAuth), Arc::new(LocalResolver)).unwrap());
    let config = SyncServerConfig {
        host: "127.0.0.1".parse().unwrap(),
        port: sync_port,
        base_folder: tmp.path().to_path_buf(),
        ip_header: default_ip_header(),
        internal_port: side_port,
        internal_host: "127.0.0.1".parse().unwrap(),
        internal_token: Some("tok".into()),
    };
    rt.block_on(async {
        let (_a, fut) = SimpleServer::make_server(config, Arc::clone(&server)).await.unwrap();
        tokio::spawn(fut);
        let sidecar = InternalServer::new(Arc::clone(&server), "tok".into());
        tokio::spawn(async move { sidecar.run("127.0.0.1".parse().unwrap(), side_port).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let api = Api { c: reqwest::Client::new(), base: format!("http://127.0.0.1:{side_port}") };

        // add: the stored name follows Anki's rules and is returned
        let image = png(640, 480);
        let (st, r) = api.add("Cat%3F.PNG", image.clone()).await;
        assert_eq!((st, r["name"].as_str()), (201, Some("cat.png")), "{r}");
        assert_eq!(r["mimeType"], "image/png");
        assert_eq!(r["added"], true);

        // same bytes again: 200, nothing added
        let (st, r) = api.add("cat.png", image.clone()).await;
        assert_eq!((st, r["added"].as_bool()), (200, Some(false)), "{r}");

        // list
        let (st, r) = api.get_json("/internal/v1/media").await;
        assert_eq!(st, 200, "{r}");
        assert_eq!(r["files"][0]["name"], "cat.png");
        assert_eq!(r["files"][0]["size"], image.len());
        assert!(r["files"][0].get("sha1").is_none() && r["files"][0].get("csum").is_none());

        // download: bytes and content type, nosniff
        let resp = api.req(reqwest::Method::GET, "/internal/v1/media/cat.png").send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "image/png");
        assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
        assert_eq!(resp.bytes().await.unwrap().as_ref(), image.as_slice());

        // metadata: facts only
        let (st, m) = api.get_json("/internal/v1/media/cat.png/metadata").await;
        assert_eq!(st, 200, "{m}");
        assert_eq!(m["mimeType"], "image/png");
        assert_eq!((m["width"].as_u64(), m["height"].as_u64()), (Some(640), Some(480)));
        assert!(m.get("sha1").is_none() && m.get("path").is_none());

        // content rules: sniffed type, not the name
        let (st, r) = api.add("evil.png", b"<script>alert(1)</script>".to_vec()).await;
        assert_eq!(st, 415, "{r}");
        // SVG is accepted but sanitised before it is stored
        let dirty = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4"><script>alert(1)</script><rect width="2" height="2"/></svg>"#;
        let (st, r) = api.add("Diagram.svg", dirty.to_vec()).await;
        assert_eq!((st, r["name"].as_str(), r["mimeType"].as_str()), (201, Some("diagram.svg"), Some("image/svg+xml")), "{r}");
        let resp = api.req(reqwest::Method::GET, "/internal/v1/media/diagram.svg").send().await.unwrap();
        assert_eq!(resp.headers()["content-type"], "image/svg+xml");
        assert!(resp.headers()["content-security-policy"].to_str().unwrap().contains("sandbox"));
        let body = resp.text().await.unwrap();
        assert!(body.contains("<rect") && !body.contains("script") && !body.contains("alert"), "{body}");
        let (st, m) = api.get_json("/internal/v1/media/diagram.svg/metadata").await;
        assert_eq!((st, m["mimeType"].as_str()), (200, Some("image/svg+xml")), "{m}");
        let (st, _) = api.add("junk.svg", b"<svg><rect></svg>".to_vec()).await;
        assert_eq!(st, 400);
        let mut big_svg = br#"<svg xmlns="http://www.w3.org/2000/svg">"#.to_vec();
        big_svg.extend(std::iter::repeat(b' ').take(201 * 1024));
        big_svg.extend(b"</svg>");
        let (st, _) = api.add("big.svg", big_svg).await;
        assert_eq!(st, 413);
        assert_eq!(api.req(reqwest::Method::DELETE, "/internal/v1/media/diagram.svg").send().await.unwrap().status(), 204);
        let (st, _) = api.add("e.png", vec![]).await;
        assert_eq!(st, 400);
        let mut big = png(1, 1);
        big.resize(5 * 1024 * 1024 + 1, 0);
        let (st, _) = api.add("big.png", big).await;
        assert_eq!(st, 413);

        // unknown names, and a path that tries to leave the media folder
        let (st, _) = api.get_json("/internal/v1/media/nope.png").await;
        assert_eq!(st, 404);
        let (st, _) = api.get_json("/internal/v1/media/..%2Fcollection.anki2").await;
        assert_eq!(st, 404);

        // delete
        let del = |n: &str| api.req(reqwest::Method::DELETE, &format!("/internal/v1/media/{n}")).send();
        assert_eq!(del("cat.png").await.unwrap().status(), 204);
        assert_eq!(del("cat.png").await.unwrap().status(), 404);
        let (_, r) = api.get_json("/internal/v1/media").await;
        assert!(r["files"].as_array().unwrap().is_empty(), "{r}");
    });
}

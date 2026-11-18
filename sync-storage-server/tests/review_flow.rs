//! End-to-end test of the sidecar review endpoints (`/internal/v1/review/*`):
//! add notes, fetch due cards, answer one, check it left the queue.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anki::sync::http_server::default_ip_header;
use anki::sync::http_server::SimpleServer;
use anki::sync::http_server::SyncServerConfig;
use serde_json::json;
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
    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.req(reqwest::Method::GET, path).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self.req(reqwest::Method::POST, path).json(&body).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
}

#[test]
fn review_next_answer_flow() {
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

        let (st, deck) = api.post("/internal/v1/decks", json!({"name": "Rust"})).await;
        assert_eq!(st, 201, "{deck}");
        let deck_id = deck["id"].as_str().unwrap().to_string();
        let notes: Vec<Value> = (1..=3)
            .map(|i| json!({"fields": {"Front": format!("<b>Question {i}</b>"), "Back": format!("Answer {i}")}}))
            .collect();
        let (st, r) = api
            .post(&format!("/internal/v1/decks/{deck_id}/notes/bulk"), json!({"notes": notes}))
            .await;
        assert_eq!(st, 201, "{r}");

        // counts
        let (st, counts) = api.get(&format!("/internal/v1/review/counts?deckId={deck_id}")).await;
        assert_eq!(st, 200, "{counts}");
        assert_eq!(counts["new"], 3, "{counts}");

        // next: limit respected, content rendered as plain text, answer has no question prefix
        let (st, next) = api.get(&format!("/internal/v1/review/next?deckId={deck_id}&limit=2")).await;
        assert_eq!(st, 200, "{next}");
        let cards = next["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 2, "{next}");
        let first = &cards[0];
        let first_id = first["cardId"].as_str().unwrap().to_string();
        assert!(first["question"].as_str().unwrap().starts_with("Question "), "{first}");
        assert!(first["answer"].as_str().unwrap().starts_with("Answer "), "{first}");
        assert!(!first["answer"].as_str().unwrap().contains("Question"), "{first}");
        assert_eq!(first["state"], "new");
        assert!(first["nextIntervals"]["good"].is_string(), "{first}");

        // limit is capped
        let (_, big) = api.get(&format!("/internal/v1/review/next?deckId={deck_id}&limit=1000")).await;
        assert!(big["cards"].as_array().unwrap().len() <= 20);

        // answering removes the card from the head of the queue
        let (st, ans) = api
            .post("/internal/v1/review/answer", json!({"cardId": first_id, "rating": "good", "millisecondsTaken": 4200}))
            .await;
        assert_eq!(st, 200, "{ans}");
        assert_eq!(ans["ok"], true);
        let (_, after) = api.get(&format!("/internal/v1/review/next?deckId={deck_id}&limit=5")).await;
        let ids: Vec<&str> = after["cards"].as_array().unwrap().iter().map(|c| c["cardId"].as_str().unwrap()).collect();
        assert_ne!(ids[0], first_id, "answered card must not come back first: {after}");
        assert_eq!(after["counts"]["new"], 2, "{after}");

        // errors
        let (st, _) = api
            .post("/internal/v1/review/answer", json!({"cardId": first_id, "rating": "meh"}))
            .await;
        assert_eq!(st, 400);
        let (st, _) = api
            .post("/internal/v1/review/answer", json!({"cardId": "12345", "rating": "good"}))
            .await;
        assert_eq!(st, 404);
        let (st, _) = api.get("/internal/v1/review/next?deckId=999999").await;
        assert_eq!(st, 404);
    });
    rt.shutdown_background();
}

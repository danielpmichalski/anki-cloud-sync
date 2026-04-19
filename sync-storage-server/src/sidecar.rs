// InternalServer — moved from rslib/src/sync/http_server/internal_server.rs

use std::net::IpAddr;
use std::sync::Arc;

use anki::sync::http_server::SimpleServer;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use tokio::net::TcpListener;

use crate::handlers;

pub struct InternalServer {
    server: Arc<SimpleServer>,
    token: String,
}

impl InternalServer {
    pub fn new(server: Arc<SimpleServer>, token: String) -> Self {
        Self { server, token }
    }

    pub async fn run(self, host: IpAddr, port: u16) {
        let token = self.token.clone();
        let app = Router::new()
            .route(
                "/internal/v1/decks",
                get(handlers::list_decks).post(handlers::create_deck),
            )
            .route(
                "/internal/v1/decks/{id}",
                get(handlers::get_deck).delete(handlers::delete_deck),
            )
            .route(
                "/internal/v1/decks/{id}/notes",
                get(handlers::list_notes).post(handlers::create_note),
            )
            .route(
                "/internal/v1/decks/{id}/notes/bulk",
                post(handlers::bulk_create_notes),
            )
            .route("/internal/v1/notes/search", get(handlers::search_notes))
            .route(
                "/internal/v1/notes/{id}",
                get(handlers::get_note)
                    .put(handlers::update_note)
                    .delete(handlers::delete_note),
            )
            .route(
                "/internal/v1/note-types",
                get(handlers::list_note_types),
            )
            .route(
                "/internal/v1/note-types/{id}",
                get(handlers::get_note_type),
            )
            .layer(middleware::from_fn(move |req, next| {
                let token = token.clone();
                validate_token(req, next, token)
            }))
            .with_state(self.server);

        let addr = format!("{host}:{port}");
        let listener = TcpListener::bind(&addr).await.unwrap();
        tracing::info!("internal sidecar listening on {addr}");
        axum::serve(listener, app).await.unwrap();
    }
}

async fn validate_token(req: Request, next: Next, token: String) -> Response {
    let provided = req
        .headers()
        .get("X-Internal-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided != token {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(req).await
}

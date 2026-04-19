// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::sync::Arc;

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

use crate::sync::http_server::internal_handlers;
use crate::sync::http_server::SimpleServer;

pub struct InternalServer {
    server: Arc<SimpleServer>,
    token: String,
}

impl InternalServer {
    pub fn new(server: Arc<SimpleServer>, token: String) -> Self {
        Self { server, token }
    }

    pub async fn run(self, port: u16) {
        let token = self.token.clone();
        let app = Router::new()
            .route(
                "/internal/v1/decks",
                get(internal_handlers::list_decks).post(internal_handlers::create_deck),
            )
            .route(
                "/internal/v1/decks/:id",
                get(internal_handlers::get_deck).delete(internal_handlers::delete_deck),
            )
            .route(
                "/internal/v1/decks/:id/notes",
                get(internal_handlers::list_notes).post(internal_handlers::create_note),
            )
            .route(
                "/internal/v1/decks/:id/notes/bulk",
                post(internal_handlers::bulk_create_notes),
            )
            .route(
                "/internal/v1/notes/search",
                get(internal_handlers::search_notes),
            )
            .route(
                "/internal/v1/notes/:id",
                get(internal_handlers::get_note)
                    .put(internal_handlers::update_note)
                    .delete(internal_handlers::delete_note),
            )
            .layer(middleware::from_fn(move |req, next| {
                let token = token.clone();
                validate_token(req, next, token)
            }))
            .with_state(self.server);

        let addr = format!("127.0.0.1:{port}");
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

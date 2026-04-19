// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use crate::notes::AddNoteRequest;
use crate::notes::Note;
use crate::prelude::*;
use crate::search::SortMode;
use crate::sync::error::HttpResult;
use crate::sync::error::OrHttpErr;
use crate::sync::http_server::user::User;
use crate::sync::http_server::SimpleServer;

fn user_email(headers: &HeaderMap) -> &str {
    headers
        .get("X-User-Email")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn err_response(status: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": msg.to_string()})))
}

/// Lock state, look up/create the user, check no sync is active, then run op.
/// Returns 409 if a sync is in progress — caller must not abort it.
fn with_user<F, R>(server: &Arc<SimpleServer>, email: &str, op: F) -> HttpResult<R>
where
    F: FnOnce(&mut User) -> HttpResult<R>,
{
    let mode = server.mode();
    let mut state = server.state.lock().unwrap();
    let user = state.get_or_create_sidecar_user(email, &server.base_folder, mode)?;
    if user.sync_state.is_some() {
        return None.or_conflict("sync in progress, try again later")?;
    }
    op(user)
}

fn note_to_json(note: &Note, col: &mut Collection) -> Value {
    let field_names: Vec<String> = col
        .get_notetype(note.notetype_id)
        .ok()
        .flatten()
        .map(|nt| nt.fields.iter().map(|f| f.name.clone()).collect())
        .unwrap_or_default();

    let fields: serde_json::Map<String, Value> = note
        .fields()
        .iter()
        .enumerate()
        .map(|(i, val)| {
            let name = field_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("Field{i}"));
            (name, Value::String(val.clone()))
        })
        .collect();

    json!({
        "id": note.id.0.to_string(),
        "noteTypeId": note.notetype_id.0.to_string(),
        "tags": note.tags,
        "fields": fields,
    })
}

// ---- Pagination helpers ----

#[derive(Deserialize, Default)]
pub struct PaginationQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    cursor: Option<String>,
}

fn default_limit() -> usize {
    100
}

const MAX_LIMIT: usize = 1000;

fn parse_cursor(cursor: &Option<String>) -> HttpResult<Option<i64>> {
    match cursor {
        None => Ok(None),
        Some(s) => s.parse::<i64>().map(Some).or_bad_request("invalid cursor"),
    }
}

// ---- Decks (read-only: with_col, no upload) ----

pub async fn list_decks(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Query(pagination): Query<PaginationQuery>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col(|col| {
            let cursor_id = parse_cursor(&pagination.cursor)?;
            let limit = pagination.limit.min(MAX_LIMIT);

            let mut decks = col
                .get_all_deck_names(false)
                .or_internal_err("list decks")?;
            decks.sort_by_key(|(id, _)| id.0);
            if let Some(c) = cursor_id {
                decks.retain(|(id, _)| id.0 > c);
            }

            let has_more = decks.len() > limit;
            let page: Vec<_> = decks.into_iter().take(limit).collect();
            let next_cursor = if has_more {
                page.last().map(|(id, _)| id.0.to_string())
            } else {
                None
            };

            let deck_json: Vec<_> = page
                .into_iter()
                .map(|(id, name)| json!({"id": id.0.to_string(), "name": name}))
                .collect();
            Ok((deck_json, next_cursor))
        })
    });
    match result {
        Ok((decks, next_cursor)) => {
            Json(json!({"decks": decks, "nextCursor": next_cursor})).into_response()
        }
        Err(e) => err_response(e.code, e).into_response(),
    }
}

pub async fn get_deck(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col(|col| col.get_deck(DeckId(id)).or_internal_err("get deck"))
    });
    match result {
        Ok(Some(deck)) => {
            Json(json!({"id": deck.id.0.to_string(), "name": deck.name.human_name()}))
                .into_response()
        }
        Ok(None) => err_response(StatusCode::NOT_FOUND, "deck not found").into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

// ---- Decks (write: with_col_and_commit) ----

#[derive(Deserialize)]
pub struct CreateDeckBody {
    name: String,
}

pub async fn create_deck(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Json(body): Json<CreateDeckBody>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col_and_commit(|col| {
            col.get_or_create_normal_deck(&body.name)
                .map(|deck| json!({"id": deck.id.0.to_string(), "name": deck.name.human_name()}))
                .or_internal_err("create deck")
        })
    });
    match result {
        Ok(deck) => (StatusCode::CREATED, Json(deck)).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

pub async fn delete_deck(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col_and_commit(|col| {
            col.remove_decks_and_child_decks(&[DeckId(id)])
                .map(|_| ())
                .or_internal_err("delete deck")
        })
    });
    match result {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

// ---- Notes (read-only: with_col) ----

pub async fn list_notes(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(deck_id): Path<i64>,
    Query(pagination): Query<PaginationQuery>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col(|col| {
            let cursor_id = parse_cursor(&pagination.cursor)?;
            let limit = pagination.limit.min(MAX_LIMIT);

            let query = format!("did:{deck_id}");
            let mut nids = col
                .search_notes_unordered(&query)
                .or_internal_err("search notes")?;
            nids.sort_by_key(|n| n.0);
            if let Some(c) = cursor_id {
                nids.retain(|n| n.0 > c);
            }

            let has_more = nids.len() > limit;
            let page_nids: Vec<_> = nids.into_iter().take(limit).collect();
            let next_cursor = if has_more {
                page_nids.last().map(|n| n.0.to_string())
            } else {
                None
            };

            let raw: Vec<Note> = page_nids
                .iter()
                .filter_map(|nid| col.storage.get_note(*nid).ok().flatten())
                .collect();
            Ok((
                raw.iter().map(|n| note_to_json(n, col)).collect::<Vec<_>>(),
                next_cursor,
            ))
        })
    });
    match result {
        Ok((notes, next_cursor)) => {
            Json(json!({"notes": notes, "nextCursor": next_cursor})).into_response()
        }
        Err(e) => err_response(e.code, e).into_response(),
    }
}

pub async fn get_note(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col(|col| {
            let note = col.storage.get_note(NoteId(id)).or_internal_err("get note")?;
            Ok(note.map(|n| note_to_json(&n, col)))
        })
    });
    match result {
        Ok(Some(note_json)) => Json(note_json).into_response(),
        Ok(None) => err_response(StatusCode::NOT_FOUND, "note not found").into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
    cursor: Option<String>,
}

pub async fn search_notes(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col(|col| {
            let cursor_id = parse_cursor(&params.cursor)?;
            let limit = params.limit.min(MAX_LIMIT);

            let mut nids = col
                .search_notes(&params.q, SortMode::NoOrder)
                .or_internal_err("search notes")?;
            nids.sort_by_key(|n| n.0);
            if let Some(c) = cursor_id {
                nids.retain(|n| n.0 > c);
            }

            let has_more = nids.len() > limit;
            let page_nids: Vec<_> = nids.into_iter().take(limit).collect();
            let next_cursor = if has_more {
                page_nids.last().map(|n| n.0.to_string())
            } else {
                None
            };

            let raw: Vec<Note> = page_nids
                .iter()
                .filter_map(|nid| col.storage.get_note(*nid).ok().flatten())
                .collect();
            Ok((
                raw.iter().map(|n| note_to_json(n, col)).collect::<Vec<_>>(),
                next_cursor,
            ))
        })
    });
    match result {
        Ok((notes, next_cursor)) => {
            Json(json!({"notes": notes, "nextCursor": next_cursor})).into_response()
        }
        Err(e) => err_response(e.code, e).into_response(),
    }
}

// ---- Notes (write: with_col_and_commit) ----

#[derive(Deserialize)]
pub struct CreateNoteBody {
    fields: std::collections::HashMap<String, String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(rename = "noteTypeId")]
    note_type_id: Option<String>,
}

pub async fn create_note(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(deck_id): Path<i64>,
    Json(body): Json<CreateNoteBody>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col_and_commit(|col| {
            let nt = if let Some(ntid_str) = &body.note_type_id {
                let ntid = ntid_str
                    .parse::<i64>()
                    .map(NotetypeId)
                    .or_bad_request("invalid noteTypeId")?;
                OrHttpErr::or_not_found(
                    col.get_notetype(ntid).or_internal_err("get notetype")?,
                    "notetype not found",
                )?
            } else {
                let all = col.get_all_notetypes().or_internal_err("list notetypes")?;
                all.into_iter()
                    .find(|nt| nt.name == "Basic")
                    .or_else(|| col.get_all_notetypes().ok().and_then(|mut v| (!v.is_empty()).then(|| v.remove(0))))
                    .or_bad_request("no notetypes found")?
            };

            let mut note = Note::new(&nt);
            note.tags = body.tags.clone();
            for (i, field) in nt.fields.iter().enumerate() {
                if let Some(val) = body.fields.get(&field.name) {
                    note.set_field(i, val).or_internal_err("set field")?;
                }
            }
            col.add_note(&mut note, DeckId(deck_id))
                .or_internal_err("add note")?;
            Ok(json!({"id": note.id.0.to_string()}))
        })
    });
    match result {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct BulkCreateNotesBody {
    notes: Vec<CreateNoteBody>,
}

pub async fn bulk_create_notes(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(deck_id): Path<i64>,
    Json(body): Json<BulkCreateNotesBody>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col_and_commit(|col| {
            let mut requests: Vec<AddNoteRequest> = Vec::with_capacity(body.notes.len());
            for note_body in &body.notes {
                let nt = if let Some(ntid_str) = &note_body.note_type_id {
                    let ntid = ntid_str
                        .parse::<i64>()
                        .map(NotetypeId)
                        .or_bad_request("invalid noteTypeId")?;
                    OrHttpErr::or_not_found(
                        col.get_notetype(ntid).or_internal_err("get notetype")?,
                        "notetype not found",
                    )?
                } else {
                    let all = col.get_all_notetypes().or_internal_err("list notetypes")?;
                    all.into_iter()
                        .find(|nt| nt.name == "Basic")
                        .or_else(|| {
                            col.get_all_notetypes()
                                .ok()
                                .and_then(|mut v| (!v.is_empty()).then(|| v.remove(0)))
                        })
                        .or_bad_request("no notetypes found")?
                };
                let mut note = Note::new(&nt);
                note.tags = note_body.tags.clone();
                for (i, field) in nt.fields.iter().enumerate() {
                    if let Some(val) = note_body.fields.get(&field.name) {
                        note.set_field(i, val).or_internal_err("set field")?;
                    }
                }
                requests.push(AddNoteRequest {
                    note,
                    deck_id: DeckId(deck_id),
                });
            }
            col.add_notes(&mut requests).or_internal_err("add notes")?;
            let ids: Vec<String> = requests.iter().map(|r| r.note.id.0.to_string()).collect();
            Ok(json!({"ids": ids}))
        })
    });
    match result {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateNoteBody {
    fields: std::collections::HashMap<String, String>,
    #[serde(default)]
    tags: Vec<String>,
}

pub async fn update_note(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<UpdateNoteBody>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col_and_commit(|col| {
            let mut note = OrHttpErr::or_not_found(
                col.storage.get_note(NoteId(id)).or_internal_err("get note")?,
                "note not found",
            )?;
            note.tags = body.tags.clone();
            let nt = OrHttpErr::or_not_found(
                col.get_notetype(note.notetype_id).or_internal_err("get notetype")?,
                "notetype not found",
            )?;
            for (i, field) in nt.fields.iter().enumerate() {
                if let Some(val) = body.fields.get(&field.name) {
                    note.set_field(i, val).or_internal_err("set field")?;
                }
            }
            col.update_note(&mut note).or_internal_err("update note")?;
            Ok(json!({"ok": true}))
        })
    });
    match result {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

pub async fn delete_note(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |user| {
        user.with_col_and_commit(|col| {
            col.remove_notes(&[NoteId(id)])
                .map(|_| ())
                .or_internal_err("delete note")
        })
    });
    match result {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

// Sidecar review (spaced-repetition) handlers: fetch due cards and record answers.
//
// All scheduling is done by the upstream `rslib` scheduler (queue building, FSRS/SM-2
// state transitions, revlog), so reviews done here behave exactly like reviews done in
// the Anki clients and reach them through the normal sync.

use std::sync::Arc;

use anki::card::CardId;
use anki::decks::DeckId;
use anki::prelude::*;
use anki::scheduler::answering::CardAnswer;
use anki::scheduler::answering::Rating;
use anki::scheduler::states::CardState;
use anki::sync::error::HttpResult;
use anki::sync::error::OrHttpErr;
use anki::sync::http_server::SimpleServer;
use anki::text::strip_html_preserving_media_filenames;
use anki::timestamp::TimestampMillis;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::handlers::err_response;
use crate::handlers::user_email;
use crate::handlers::with_user;

/// Hard cap on cards per request: keeps the LLM's context small.
const MAX_REVIEW_LIMIT: usize = 20;
const DEFAULT_REVIEW_LIMIT: usize = 5;

fn default_limit() -> usize {
    DEFAULT_REVIEW_LIMIT
}

/// Plain text for the LLM: drop markup but keep media filenames.
fn plain(html: &str) -> String {
    let text = strip_html_preserving_media_filenames(html);
    text.trim().to_string()
}

/// The rendered answer side repeats the question ({{FrontSide}}) before `<hr id=answer>`;
/// keep only what follows it.
fn answer_part(answer_html: &str) -> &str {
    match answer_html.find("id=answer").or_else(|| answer_html.find("id=\"answer\"")) {
        Some(pos) => match answer_html[pos..].find('>') {
            Some(end) => &answer_html[pos + end + 1..],
            None => answer_html,
        },
        None => answer_html,
    }
}

/// Run `op` with the collection's queue built for `deck_id`, then restore the user's
/// previously selected deck (a study session must not change what the Anki clients
/// show as the current deck).
///
/// The scheduler builds its queue for the *current deck*, and card answers never
/// invalidate a cached queue. Switching decks through a sentinel id guarantees a
/// fresh queue on every call.
fn with_deck_queue<R>(
    col: &mut Collection,
    deck_id: DeckId,
    op: impl FnOnce(&mut Collection) -> Result<R>,
) -> Result<R> {
    let previous = col.get_current_deck()?.id;
    col.set_current_deck(DeckId(0))?;
    col.set_current_deck(deck_id)?;
    let result = op(col);
    // restoring also drops the queue again, so no stale queue survives this request
    let restore = col.set_current_deck(previous);
    let result = result?;
    restore?;
    Ok(result)
}

#[derive(Deserialize)]
pub struct DeckQuery {
    #[serde(rename = "deckId")]
    deck_id: i64,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn require_deck(col: &mut Collection, id: i64) -> HttpResult<DeckId> {
    OrHttpErr::or_not_found(
        col.get_deck(DeckId(id)).or_internal_err("get deck")?,
        "deck not found",
    )?;
    Ok(DeckId(id))
}

pub async fn next_cards(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Query(q): Query<DeckQuery>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let limit = q.limit.min(MAX_REVIEW_LIMIT);
    let result = with_user(&server, &email, move |handle| {
        handle.with_col(|col| {
            let deck_id = require_deck(col, q.deck_id)?;
            with_deck_queue(col, deck_id, |col| {
                let queued = col.get_queued_cards(limit, false)?;
                let counts = json!({
                    "new": queued.new_count,
                    "learning": queued.learning_count,
                    "review": queued.review_count,
                });
                let mut cards = Vec::with_capacity(queued.cards.len());
                for qc in &queued.cards {
                    let rendered = col.render_existing_card(qc.card.id(), false, false)?;
                    let labels = col.describe_next_states(&qc.states)?;
                    let deck_name = col
                        .get_deck(qc.card.deck_id())?
                        .map(|d| d.name.human_name())
                        .unwrap_or_default();
                    let note = col.storage.get_note(qc.card.note_id())?;
                    let tags = note.map(|n| n.tags).unwrap_or_default();
                    cards.push(json!({
                        "cardId": qc.card.id().0.to_string(),
                        "noteId": qc.card.note_id().0.to_string(),
                        "deck": deck_name,
                        "state": format!("{:?}", qc.kind).to_lowercase(),
                        "tags": tags,
                        "question": plain(&rendered.question()),
                        "answer": plain(answer_part(&rendered.answer())),
                        "nextIntervals": {
                            "again": labels.first(),
                            "hard": labels.get(1),
                            "good": labels.get(2),
                            "easy": labels.get(3),
                        },
                    }));
                }
                Ok(json!({"cards": cards, "counts": counts}))
            })
            .or_internal_err("build review queue")
        })
    })
    .await;
    match result {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

pub async fn review_counts(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Query(q): Query<DeckQuery>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, move |handle| {
        handle.with_col(|col| {
            let deck_id = require_deck(col, q.deck_id)?;
            with_deck_queue(col, deck_id, |col| {
                let queued = col.get_queued_cards(0, false)?;
                Ok(json!({
                    "new": queued.new_count,
                    "learning": queued.learning_count,
                    "review": queued.review_count,
                }))
            })
            .or_internal_err("build review queue")
        })
    })
    .await;
    match result {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct AnswerBody {
    #[serde(rename = "cardId")]
    card_id: String,
    rating: String,
    #[serde(rename = "millisecondsTaken", default)]
    milliseconds_taken: u32,
}

fn parse_rating(s: &str) -> Option<Rating> {
    match s.to_ascii_lowercase().as_str() {
        "again" | "1" => Some(Rating::Again),
        "hard" | "2" => Some(Rating::Hard),
        "good" | "3" => Some(Rating::Good),
        "easy" | "4" => Some(Rating::Easy),
        _ => None,
    }
}

pub async fn answer_card(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Json(body): Json<AnswerBody>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let Some(rating) = parse_rating(&body.rating) else {
        return err_response(
            StatusCode::BAD_REQUEST,
            "rating must be one of: again, hard, good, easy",
        )
        .into_response();
    };
    let Ok(card_id) = body.card_id.parse::<i64>() else {
        return err_response(StatusCode::BAD_REQUEST, "invalid cardId").into_response();
    };
    let millis = body.milliseconds_taken;
    let result = with_user(&server, &email, move |handle| {
        handle.with_col_and_commit(|col| {
            let cid = CardId(card_id);
            if col
                .storage
                .get_card(cid)
                .or_internal_err("get card")?
                .is_none()
            {
                return Err(err_not_found("card not found"));
            }
            let states = col.get_scheduling_states(cid).or_internal_err("get states")?;
            let labels = col.describe_next_states(&states).or_internal_err("describe")?;
            let (new_state, label): (CardState, Option<String>) = match rating {
                Rating::Again => (states.again, labels.first().cloned()),
                Rating::Hard => (states.hard, labels.get(1).cloned()),
                Rating::Good => (states.good, labels.get(2).cloned()),
                Rating::Easy => (states.easy, labels.get(3).cloned()),
            };
            let mut answer = CardAnswer {
                card_id: cid,
                current_state: states.current,
                new_state,
                rating,
                answered_at: TimestampMillis::now(),
                milliseconds_taken: millis,
                custom_data: None,
                // the queue is rebuilt on every /review/next, so it must not be touched here
                from_queue: false,
            };
            col.answer_card(&mut answer).or_internal_err("answer card")?;
            Ok(json!({"ok": true, "next": label}))
        })
    })
    .await;
    match result {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

fn err_not_found(msg: &str) -> anki::sync::error::HttpError {
    OrHttpErr::or_not_found(None::<()>, msg).unwrap_err()
}

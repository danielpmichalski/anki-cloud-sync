// Sidecar (internal API) media endpoints. The REST layer calls these; they sit on the
// server-side media code (`SidecarUserHandle::{add,list,get,delete}_media`), so a file added here
// reaches clients through the normal media sync and lives in the user's storage.
//
// Content rules live here: files are identified by their bytes (never by the extension the
// caller claims), only an allowlist of types is accepted, and sizes are capped.

use std::sync::Arc;

use anki::sync::http_server::SimpleServer;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::handlers::err_response;
use crate::handlers::user_email;
use crate::handlers::with_user;

pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// SVG is text, so it also costs the model's context; larger drawings go through the upload link.
pub const MAX_SVG_BYTES: usize = 200 * 1024;
pub const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
/// Request body cap: the largest allowed file.
pub const MAX_BODY_BYTES: usize = MAX_AUDIO_BYTES;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    Image,
    /// SVG is text and can carry scripts, so it is sanitised before it is stored.
    Svg,
    Audio,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Sniffed {
    pub mime: &'static str,
    pub kind: Kind,
}

const fn s(mime: &'static str, kind: Kind) -> Option<Sniffed> {
    Some(Sniffed { mime, kind })
}

/// Identify the file from its first bytes (the `infer` crate), never from the name the caller
/// gives. Only the allowlisted types come back; everything else, including video, is `None`.
pub fn sniff(b: &[u8]) -> Option<Sniffed> {
    // 3GP is what AnkiDroid records voice notes as. `infer` has no matcher for it; the brand in
    // the `ftyp` box starts with "3gp". (A 3GP *video* has the same brand: the container cannot
    // tell them apart without parsing its tracks, and Anki plays both.)
    if b.len() >= 12 && &b[4..8] == b"ftyp" && &b[8..11] == b"3gp" {
        return s("audio/3gpp", Kind::Audio);
    }
    match infer::get(b)?.mime_type() {
        "image/png" => s("image/png", Kind::Image),
        "image/jpeg" => s("image/jpeg", Kind::Image),
        "image/gif" => s("image/gif", Kind::Image),
        "image/webp" => s("image/webp", Kind::Image),
        "audio/mpeg" => s("audio/mpeg", Kind::Audio),
        "audio/ogg" => s("audio/ogg", Kind::Audio),
        "audio/x-wav" => s("audio/wav", Kind::Audio),
        "audio/m4a" => s("audio/mp4", Kind::Audio),
        "audio/amr" => s("audio/amr", Kind::Audio),
        _ => None,
    }
}

/// SVG has no magic number. This is only a cheap pre-check; `sanitize_svg` is the real test (the
/// file must parse as XML with an SVG root).
fn looks_like_svg(b: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&b[..b.len().min(2048)]).into_owned();
    head.contains("<svg")
}

/// Run the file through svg-hush, an allowlist filter: scripts, event handlers, foreign objects
/// and every reference to another origin are dropped. Stored SVG is rendered inside the Anki
/// clients' webviews, and its text reaches the model, so it must be harmless before it is saved.
pub fn sanitize_svg(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len());
    svg_hush::Filter::new()
        .filter(data, &mut out)
        .map_err(|e| format!("not a valid SVG: {e}"))?;
    if !looks_like_svg(&out) {
        return Err("not a valid SVG".into());
    }
    Ok(out)
}

/// MIME type of a stored file, including SVG (which has no magic number).
fn detect_mime(data: &[u8]) -> Option<&'static str> {
    match sniff(data) {
        Some(s) => Some(s.mime),
        None if looks_like_svg(data) => Some("image/svg+xml"),
        None => None,
    }
}

fn dimensions(data: &[u8], sniffed: Sniffed) -> Option<(usize, usize)> {
    if sniffed.kind != Kind::Image {
        return None;
    }
    imagesize::blob_size(data).ok().map(|d| (d.width, d.height))
}

#[derive(Deserialize)]
pub struct AddQuery {
    name: String,
}

/// `POST /internal/v1/media?name=cat.png`, body = the file's bytes.
pub async fn add_media(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Query(q): Query<AddQuery>,
    body: Bytes,
) -> impl IntoResponse {
    if body.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "empty file").into_response();
    }
    let mut data = body.to_vec();
    let sniffed = match sniff(&data) {
        Some(sniffed) => sniffed,
        None if looks_like_svg(&data) => {
            if data.len() > MAX_SVG_BYTES {
                return err_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("SVG too large: {} bytes, the limit is {MAX_SVG_BYTES}", data.len()),
                )
                .into_response();
            }
            data = match sanitize_svg(&data) {
                Ok(clean) => clean,
                Err(msg) => return err_response(StatusCode::BAD_REQUEST, msg).into_response(),
            };
            Sniffed { mime: "image/svg+xml", kind: Kind::Svg }
        }
        None => {
            return err_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported file type (png, jpg, gif, webp, svg, mp3, ogg, wav, m4a, 3gp and amr are accepted)",
            )
            .into_response();
        }
    };
    let limit = match sniffed.kind {
        Kind::Image => MAX_IMAGE_BYTES,
        Kind::Svg => MAX_SVG_BYTES,
        Kind::Audio => MAX_AUDIO_BYTES,
    };
    if data.len() > limit {
        return err_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("file too large: {} bytes, the limit for this type is {limit}", data.len()),
        )
        .into_response();
    }

    let email = user_email(&headers).to_string();
    let size = data.len();
    let result = with_user(&server, &email, move |handle| handle.add_media(&q.name, data)).await;
    match result {
        Ok(stored) => {
            let status = if stored.added { StatusCode::CREATED } else { StatusCode::OK };
            (
                status,
                Json(json!({
                    "name": stored.name,
                    "added": stored.added,
                    "mimeType": sniffed.mime,
                    "size": size,
                })),
            )
                .into_response()
        }
        Err(e) => err_response(e.code, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    /// Name of the last entry of the previous page.
    cursor: Option<String>,
}

fn default_limit() -> usize {
    100
}

const MAX_LIMIT: usize = 1000;

/// `GET /internal/v1/media`: name and size from `media.db`; no storage call.
pub async fn list_media(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    let result = with_user(&server, &email, |handle| handle.list_media()).await;
    match result {
        Ok(entries) => {
            let limit = q.limit.clamp(1, MAX_LIMIT);
            let mut rest = entries
                .into_iter()
                .filter(|e| q.cursor.as_deref().is_none_or(|c| e.name.as_str() > c))
                .collect::<Vec<_>>();
            let has_more = rest.len() > limit;
            rest.truncate(limit);
            let next_cursor = if has_more { rest.last().map(|e| e.name.clone()) } else { None };
            let files: Vec<_> = rest
                .into_iter()
                .map(|e| json!({"name": e.name, "size": e.size}))
                .collect();
            Json(json!({"files": files, "nextCursor": next_cursor})).into_response()
        }
        Err(e) => err_response(e.code, e).into_response(),
    }
}

async fn fetch(
    server: &Arc<SimpleServer>,
    headers: &HeaderMap,
    name: String,
) -> Result<Option<Vec<u8>>, (StatusCode, Json<serde_json::Value>)> {
    let email = user_email(headers).to_string();
    with_user(server, &email, move |handle| handle.get_media(&name))
        .await
        .map_err(|e| err_response(e.code, e))
}

/// `GET /internal/v1/media/{name}`: the bytes, fetched from storage on demand.
pub async fn get_media(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match fetch(&server, &headers, name).await {
        Ok(Some(data)) => {
            let mime = detect_mime(&data).unwrap_or("application/octet-stream");
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    // Defence in depth for SVG opened as a top-level document.
                    (header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'; sandbox"),
                ],
                data,
            )
                .into_response()
        }
        Ok(None) => err_response(StatusCode::NOT_FOUND, "media file not found").into_response(),
        Err(resp) => resp.into_response(),
    }
}

/// `GET /internal/v1/media/{name}/metadata`: facts only (no hash, no paths).
pub async fn media_metadata(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match fetch(&server, &headers, name.clone()).await {
        Ok(Some(data)) => {
            let sniffed = sniff(&data);
            let dims = sniffed.and_then(|s| dimensions(&data, s));
            let mime = detect_mime(&data);
            Json(json!({
                "name": name,
                "mimeType": mime,
                "size": data.len(),
                "width": dims.map(|d| d.0),
                "height": dims.map(|d| d.1),
            }))
            .into_response()
        }
        Ok(None) => err_response(StatusCode::NOT_FOUND, "media file not found").into_response(),
        Err(resp) => resp.into_response(),
    }
}

/// `DELETE /internal/v1/media/{name}`
pub async fn delete_media(
    State(server): State<Arc<SimpleServer>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let email = user_email(&headers).to_string();
    match with_user(&server, &email, move |handle| handle.delete_media(&name)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err_response(StatusCode::NOT_FOUND, "media file not found").into_response(),
        Err(e) => err_response(e.code, e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    #[test]
    fn sniffs_allowed_types_by_content() {
        assert_eq!(sniff(&png(1, 1)).unwrap().mime, "image/png");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap().mime, "image/jpeg");
        assert_eq!(sniff(b"GIF89a....").unwrap().mime, "image/gif");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ").unwrap().mime, "image/webp");
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt ").unwrap().mime, "audio/wav");
        assert_eq!(sniff(b"OggS\0\x02").unwrap().mime, "audio/ogg");
        assert_eq!(sniff(b"ID3\x04\0").unwrap().mime, "audio/mpeg");
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0]).unwrap().mime, "audio/mpeg");
        assert_eq!(sniff(b"\0\0\0\x20ftypM4A \0\0").unwrap().mime, "audio/mp4");
        assert_eq!(sniff(b"\0\0\0\x14ftyp3gp4\0\0\0\0").unwrap().mime, "audio/3gpp");
        assert_eq!(sniff(b"#!AMR\n\x3c\0\0\0\0\0\0").unwrap().mime, "audio/amr");
    }

    #[test]
    fn rejects_everything_else() {
        // A video in an MP4 container, a script, an executable and an SVG are none of the
        // binary types we accept.
        assert!(sniff(b"\0\0\0\x20ftypisom\0\0\0\0").is_none());
        assert!(sniff(b"<script>alert(1)</script>").is_none());
        assert!(sniff(b"MZ\x90\0").is_none());
        assert!(sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>").is_none());
        assert!(looks_like_svg(b"<?xml version=\"1.0\"?><svg/>"));
        assert!(!looks_like_svg(b"<html></html>"));
    }

    #[test]
    fn svg_is_sanitised_with_an_allowlist() {
        let dirty = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10" onload="alert(1)">
            <script>alert(2)</script>
            <foreignObject><div>hi</div></foreignObject>
            <image href="https://evil.example/track.png"/>
            <a href="https://evil.example"><text>link</text></a>
            <rect width="5" height="5" fill="red"/></svg>"#;
        let clean = String::from_utf8(sanitize_svg(dirty).unwrap()).unwrap();
        assert!(clean.contains("<rect"), "{clean}");
        assert!(clean.contains("viewBox"), "{clean}");
        for bad in ["script", "onload", "alert", "foreignObject", "evil.example"] {
            assert!(!clean.contains(bad), "{bad} survived: {clean}");
        }
    }

    #[test]
    fn broken_or_non_svg_xml_is_rejected() {
        assert!(sanitize_svg(b"<svg><rect></svg>").is_err());
        assert!(sanitize_svg(b"<html><body/></html>").is_err());
    }

    #[test]
    fn reads_image_dimensions_only_for_images() {
        let data = png(640, 480);
        let sniffed = sniff(&data).unwrap();
        assert_eq!(dimensions(&data, sniffed), Some((640, 480)));
        let audio = b"OggS\0\x02";
        assert_eq!(dimensions(audio, sniff(audio).unwrap()), None);
    }
}

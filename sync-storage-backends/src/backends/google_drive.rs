use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, RequestBuilder, Response};
use serde_json::{json, Value};
use sync_platform_api::StorageBackend;
use tokio::fs;
use tokio::io::AsyncWriteExt;

const COLLECTION_FILE_NAME: &str = "collection.anki2";
const MEDIA_INDEX_FILE_NAME: &str = "media.db";
const MEDIA_FOLDER_NAME: &str = "collection.media";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const DRIVE_FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const DRIVE_UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";

/// Chunk size for resumable uploads. Drive requires a multiple of 256 KiB.
const CHUNK_SIZE: usize = 8 * 1024 * 1024;
/// Files up to this size are sent in a single multipart request instead of a resumable session.
const MULTIPART_MAX: usize = 5 * 1024 * 1024;
const MAX_ATTEMPTS: u32 = 5;
/// Concurrent Drive requests per media batch.
const MEDIA_CONCURRENCY: usize = 8;
/// Names per `name = 'a' or name = 'b'` lookup query.
const LOOKUP_BATCH: usize = 10;

/// Folder ids resolved earlier in this process, keyed by `user|path`. Backends are created per
/// request, so without this every media request would repeat the folder lookups.
fn folder_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn build_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .expect("build http client")
}

/// One client for the whole process, so connections are reused between Drive calls. Tests build
/// their own, because each `#[tokio::test]` runs on a separate runtime.
fn shared_client() -> Client {
    if cfg!(test) {
        return build_client();
    }
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(build_client).clone()
}

/// Escape a value for use inside a single-quoted Drive query string.
fn q_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

pub struct GoogleDriveBackend {
    oauth_token: String,
    folder_name: String,
    files_base_url: String,
    upload_base_url: String,
    client: Client,
}

impl GoogleDriveBackend {
    pub fn new(oauth_token: impl Into<String>, folder_path: impl AsRef<str>) -> Self {
        Self {
            oauth_token: oauth_token.into(),
            folder_name: folder_path.as_ref().trim_start_matches('/').to_string(),
            files_base_url: DRIVE_FILES_URL.to_string(),
            upload_base_url: DRIVE_UPLOAD_URL.to_string(),
            client: shared_client(),
        }
    }

    #[cfg(test)]
    pub fn with_base_urls(
        oauth_token: impl Into<String>,
        folder_path: impl AsRef<str>,
        files_url: String,
        upload_url: String,
    ) -> Self {
        Self {
            oauth_token: oauth_token.into(),
            folder_name: folder_path.as_ref().trim_start_matches('/').to_string(),
            files_base_url: files_url,
            upload_base_url: upload_url,
            client: build_client(),
        }
    }

    fn auth_header(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {}", self.oauth_token)
                .parse()
                .expect("valid auth header"),
        );
        headers
    }

    fn backoff_duration(attempt: u32) -> Duration {
        let base_ms = (500u64 << attempt.min(6)).min(16_000);
        let jitter_ms = rand::random::<u64>() % 500;
        Duration::from_millis(base_ms + jitter_ms)
    }

    /// Send a request, retrying on rate limits, server errors and transient network failures.
    /// `build` is called once per attempt. The final response is returned as is; callers check
    /// the status with [`Self::check`].
    async fn send(&self, build: impl Fn() -> RequestBuilder) -> Result<Response> {
        let mut attempt = 0;
        loop {
            let result = build().send().await;
            let retry = match &result {
                Ok(r) => matches!(r.status().as_u16(), 403 | 429 | 500 | 502 | 503 | 504),
                Err(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            };
            if !retry || attempt >= MAX_ATTEMPTS {
                return result.map_err(Into::into);
            }
            tokio::time::sleep(Self::backoff_duration(attempt)).await;
            attempt += 1;
        }
    }

    async fn check(response: Response, what: &str) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        Err(anyhow!("Drive {what} failed: HTTP {status}: {body}"))
    }

    async fn list(&self, query: &str, fields: &str) -> Result<Vec<Value>> {
        let response = self
            .send(|| {
                self.client
                    .get(&self.files_base_url)
                    .headers(self.auth_header())
                    .query(&[
                        ("q", query),
                        ("spaces", "drive"),
                        ("fields", fields),
                        ("pageSize", "1000"),
                    ])
            })
            .await?;
        let body: Value = Self::check(response, "list").await?.json().await?;
        body.get("files")
            .and_then(|f| f.as_array())
            .cloned()
            .ok_or_else(|| anyhow!("failed to parse file list"))
    }

    async fn create_folder(&self, name: &str, parent: Option<&str>) -> Result<String> {
        let mut metadata = json!({ "name": name, "mimeType": FOLDER_MIME });
        if let Some(parent) = parent {
            metadata["parents"] = json!([parent]);
        }
        let response = self
            .send(|| {
                self.client
                    .post(&self.files_base_url)
                    .headers(self.auth_header())
                    .json(&metadata)
            })
            .await?;
        let body: Value = Self::check(response, "create folder").await?.json().await?;
        body.get("id")
            .and_then(|id| id.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("failed to create folder '{name}'"))
    }

    async fn get_or_create_anki_folder(&self) -> Result<String> {
        let query = format!(
            "name='{}' and mimeType='{FOLDER_MIME}' and trashed=false",
            q_escape(&self.folder_name)
        );
        let files = self.list(&query, "files(id)").await?;
        if let Some(id) = files.first().and_then(|f| f.get("id")).and_then(|id| id.as_str()) {
            return Ok(id.to_string());
        }
        self.create_folder(&self.folder_name, None).await
    }

    async fn get_or_create_subfolder(&self, parent_id: &str, name: &str) -> Result<String> {
        let query = format!(
            "'{parent_id}' in parents and name='{}' and mimeType='{FOLDER_MIME}' and trashed=false",
            q_escape(name)
        );
        let files = self.list(&query, "files(id)").await?;
        if let Some(id) = files.first().and_then(|f| f.get("id")).and_then(|id| id.as_str()) {
            return Ok(id.to_string());
        }
        self.create_folder(name, Some(parent_id)).await
    }

    /// The user's root folder id, cached for the life of the process.
    async fn root_folder_id(&self, user: &str) -> Result<String> {
        let key = format!("{user}|{}", self.folder_name);
        if let Some(id) = folder_cache().lock().unwrap().get(&key) {
            return Ok(id.clone());
        }
        let id = self.get_or_create_anki_folder().await?;
        folder_cache().lock().unwrap().insert(key, id.clone());
        Ok(id)
    }

    /// The `collection.media` folder id, cached for the life of the process.
    async fn media_folder_id(&self, user: &str) -> Result<String> {
        let key = format!("{user}|{}|{MEDIA_FOLDER_NAME}", self.folder_name);
        if let Some(id) = folder_cache().lock().unwrap().get(&key) {
            return Ok(id.clone());
        }
        let root = self.root_folder_id(user).await?;
        let id = self.get_or_create_subfolder(&root, MEDIA_FOLDER_NAME).await?;
        folder_cache().lock().unwrap().insert(key, id.clone());
        Ok(id)
    }

    /// Forget cached folder ids for `user`, e.g. after the user deleted a folder in Drive.
    fn forget_folders(&self, user: &str) {
        let prefix = format!("{user}|");
        folder_cache()
            .lock()
            .unwrap()
            .retain(|key, _| !key.starts_with(&prefix));
    }

    async fn find_child(&self, folder_id: &str, name: &str) -> Result<Option<String>> {
        let query = format!(
            "'{folder_id}' in parents and name='{}' and trashed=false",
            q_escape(name)
        );
        let files = self.list(&query, "files(id)").await?;
        Ok(files
            .first()
            .and_then(|f| f.get("id"))
            .and_then(|id| id.as_str())
            .map(str::to_string))
    }

    async fn find_collection_file(&self, folder_id: &str) -> Result<Option<String>> {
        self.find_child(folder_id, COLLECTION_FILE_NAME).await
    }

    /// Resolve file ids for `names` inside `folder_id`, a few names per query. Names that do not
    /// exist are absent from the result.
    async fn lookup_ids(&self, folder_id: &str, names: &[String]) -> Result<HashMap<String, String>> {
        let mut found = HashMap::new();
        for batch in names.chunks(LOOKUP_BATCH) {
            let clauses: Vec<String> = batch
                .iter()
                .map(|n| format!("name='{}'", q_escape(n)))
                .collect();
            let query = format!(
                "'{folder_id}' in parents and trashed=false and ({})",
                clauses.join(" or ")
            );
            for file in self.list(&query, "files(id,name)").await? {
                if let (Some(id), Some(name)) = (
                    file.get("id").and_then(|v| v.as_str()),
                    file.get("name").and_then(|v| v.as_str()),
                ) {
                    found.entry(name.to_string()).or_insert_with(|| id.to_string());
                }
            }
        }
        Ok(found)
    }

    async fn download_file(&self, file_id: &str, dest: &Path) -> Result<()> {
        let url = format!("{}/{}?alt=media", self.files_base_url, file_id);
        let response = self
            .send(|| self.client.get(&url).headers(self.auth_header()))
            .await?;
        let mut response = Self::check(response, "download").await?;

        // Stream to a temp file and rename, so a failure never leaves a truncated file behind.
        let mut part = dest.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let result: Result<()> = async {
            let mut file = fs::File::create(&part).await?;
            while let Some(chunk) = response.chunk().await? {
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => Ok(fs::rename(&part, dest).await?),
            Err(e) => {
                let _ = fs::remove_file(&part).await;
                Err(e)
            }
        }
    }

    /// Create or replace a file. Small files go in one multipart request, large ones in a
    /// resumable session.
    async fn upload_file(
        &self,
        name: &str,
        file_id: Option<&str>,
        folder_id: &str,
        src: &Path,
    ) -> Result<()> {
        let data = Bytes::from(fs::read(src).await?);
        let metadata = if file_id.is_some() {
            json!({ "name": name })
        } else {
            json!({ "name": name, "parents": [folder_id] })
        };
        if data.len() <= MULTIPART_MAX {
            self.upload_multipart(file_id, &metadata, data).await
        } else {
            self.upload_resumable(file_id, &metadata, data).await
        }
    }

    async fn upload_multipart(&self, file_id: Option<&str>, metadata: &Value, data: Bytes) -> Result<()> {
        let boundary = format!("ankicloud{:016x}", rand::random::<u64>());
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n\
                 --{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
        let body = Bytes::from(body);
        let content_type = format!("multipart/related; boundary={boundary}");

        let url = match file_id {
            Some(id) => format!("{}/{}", self.upload_base_url, id),
            None => self.upload_base_url.clone(),
        };
        let response = self
            .send(|| {
                let request = if file_id.is_some() {
                    self.client.patch(&url)
                } else {
                    self.client.post(&url)
                };
                request
                    .headers(self.auth_header())
                    .query(&[("uploadType", "multipart")])
                    .header(CONTENT_TYPE, content_type.as_str())
                    .body(body.clone())
            })
            .await?;
        Self::check(response, "multipart upload").await?;
        Ok(())
    }

    async fn upload_resumable(&self, file_id: Option<&str>, metadata: &Value, data: Bytes) -> Result<()> {
        let total = data.len();
        let url = match file_id {
            Some(id) => format!("{}/{}", self.upload_base_url, id),
            None => self.upload_base_url.clone(),
        };

        // Initiate the session.
        let session = self
            .send(|| {
                let request = if file_id.is_some() {
                    self.client.patch(&url)
                } else {
                    self.client.post(&url)
                };
                request
                    .headers(self.auth_header())
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .header("X-Upload-Content-Length", total.to_string())
                    .query(&[("uploadType", "resumable")])
                    .json(metadata)
            })
            .await?;
        let session = Self::check(session, "upload initiation").await?;
        let session_uri = session
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no session URI returned"))?;

        // Send the data in chunks.
        for start in (0..total).step_by(CHUNK_SIZE) {
            let end = (start + CHUNK_SIZE).min(total);
            let chunk = data.slice(start..end);
            let range = format!("bytes {}-{}/{}", start, end - 1, total);
            let response = self
                .send(|| {
                    self.client
                        .put(session_uri.as_str())
                        .header("Content-Range", range.as_str())
                        .header(CONTENT_TYPE, "application/octet-stream")
                        .body(chunk.clone())
                })
                .await?;
            match response.status().as_u16() {
                200 | 201 => return Ok(()),
                308 => continue,
                code => {
                    let body = response.text().await.unwrap_or_default();
                    return Err(anyhow!("upload failed: HTTP {code}: {body}"));
                }
            }
        }
        Err(anyhow!("upload ended without a completion response"))
    }

    /// Download a SQLite database (the collection or the media index) over `dest`.
    ///
    /// SQLite pairs a database with its `-wal`/`-shm`/`-journal` files, and a WAL written for an
    /// older copy of the database is replayed on top of a newer one without any check, which
    /// corrupts it ("database disk image is malformed").
    ///
    /// Drive is the source of truth and every reopen re-fetches, so leftovers from an earlier
    /// or killed process hold nothing worth keeping. They are removed first, so a crash never leaves
    /// a new database next to an old WAL.
    ///
    /// `clean_sidecars` is false when a live database may sit at `dest`.
    async fn download_database(
        &self,
        file_id: &str,
        dest: &Path,
        clean_sidecars: bool,
    ) -> Result<()> {
        if clean_sidecars {
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut sidecar = dest.as_os_str().to_owned();
                sidecar.push(suffix);
                match fs::remove_file(PathBuf::from(sidecar)).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.download_file(file_id, dest).await
    }

    async fn fetch_async(&self, user: &str, dest: &Path) -> Result<()> {
        let folder_id = self.root_folder_id(user).await?;
        if let Some(file_id) = self.find_collection_file(&folder_id).await? {
            self.download_database(&file_id, dest, true).await?;
        }
        Ok(())
    }

    async fn commit_async(&self, user: &str, src: &Path) -> Result<()> {
        let folder_id = self.root_folder_id(user).await?;
        let file_id = self.find_collection_file(&folder_id).await?;
        self.upload_file(COLLECTION_FILE_NAME, file_id.as_deref(), &folder_id, src)
            .await
    }

    async fn media_index_fetch_async(&self, user: &str, dest: &Path) -> Result<bool> {
        let folder_id = self.root_folder_id(user).await?;
        match self.find_child(&folder_id, MEDIA_INDEX_FILE_NAME).await? {
            Some(file_id) => {
                // The index is only fetched when there is no local copy; never touch a live one.
                let absent = !dest.exists();
                self.download_database(&file_id, dest, absent).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn media_index_commit_async(&self, user: &str, src: &Path) -> Result<()> {
        let folder_id = self.root_folder_id(user).await?;
        let file_id = self.find_child(&folder_id, MEDIA_INDEX_FILE_NAME).await?;
        self.upload_file(MEDIA_INDEX_FILE_NAME, file_id.as_deref(), &folder_id, src)
            .await
    }

    async fn media_put_many_async(&self, user: &str, files: &[(String, PathBuf)]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let folder_id = self.media_folder_id(user).await?;
        let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
        let existing = self.lookup_ids(&folder_id, &names).await?;

        let results: Vec<Result<()>> = stream::iter(files.iter())
            .map(|(name, path)| {
                let id = existing.get(name).map(String::as_str);
                self.upload_file(name, id, &folder_id, path)
            })
            .buffer_unordered(MEDIA_CONCURRENCY)
            .collect()
            .await;
        results.into_iter().collect()
    }

    async fn media_get_many_async(
        &self,
        user: &str,
        names: &[String],
        dest_dir: &Path,
    ) -> Result<Vec<String>> {
        if names.is_empty() {
            return Ok(vec![]);
        }
        let folder_id = self.media_folder_id(user).await?;
        let ids = self.lookup_ids(&folder_id, names).await?;
        let missing: Vec<String> = names.iter().filter(|n| !ids.contains_key(*n)).cloned().collect();

        let results: Vec<Result<()>> = stream::iter(names.iter().filter_map(|n| ids.get(n).map(|id| (n, id))))
            .map(|(name, id)| async move {
                let dest = dest_dir.join(name);
                self.download_file(id, &dest).await
            })
            .buffer_unordered(MEDIA_CONCURRENCY)
            .collect()
            .await;
        results.into_iter().collect::<Result<Vec<()>>>()?;
        Ok(missing)
    }

    async fn media_delete_many_async(&self, user: &str, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let folder_id = self.media_folder_id(user).await?;
        let ids = self.lookup_ids(&folder_id, names).await?;

        let results: Vec<Result<()>> = stream::iter(ids.values())
            .map(|id| async move {
                let url = format!("{}/{}", self.files_base_url, id);
                let response = self
                    .send(|| {
                        self.client
                            .patch(&url)
                            .headers(self.auth_header())
                            .json(&json!({ "trashed": true }))
                    })
                    .await?;
                if response.status().as_u16() == 404 {
                    return Ok(());
                }
                Self::check(response, "trash").await?;
                Ok(())
            })
            .buffer_unordered(MEDIA_CONCURRENCY)
            .collect()
            .await;
        results.into_iter().collect()
    }

    /// Run a media operation; on failure drop the cached folder ids so the next call
    /// re-resolves them (the user may have moved or deleted a folder in Drive).
    fn run<T>(&self, user: &str, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let result = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut));
        if result.is_err() {
            self.forget_folders(user);
        }
        result
    }
}

impl StorageBackend for GoogleDriveBackend {
    fn fetch(&self, user: &str, dest: &Path) -> Result<()> {
        self.run(user, self.fetch_async(user, dest))
    }

    fn commit(&self, user: &str, src: &Path) -> Result<()> {
        self.run(user, self.commit_async(user, src))
    }

    fn supports_media(&self) -> bool {
        true
    }

    fn media_index_fetch(&self, user: &str, dest: &Path) -> Result<bool> {
        self.run(user, self.media_index_fetch_async(user, dest))
    }

    fn media_index_commit(&self, user: &str, src: &Path) -> Result<()> {
        self.run(user, self.media_index_commit_async(user, src))
    }

    fn media_put_many(&self, user: &str, files: &[(String, PathBuf)]) -> Result<()> {
        self.run(user, self.media_put_many_async(user, files))
    }

    fn media_get_many(&self, user: &str, names: &[String], dest_dir: &Path) -> Result<Vec<String>> {
        self.run(user, self.media_get_many_async(user, names, dest_dir))
    }

    fn media_delete_many(&self, user: &str, names: &[String]) -> Result<()> {
        self.run(user, self.media_delete_many_async(user, names))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn backend(server: &MockServer) -> GoogleDriveBackend {
        GoogleDriveBackend::with_base_urls(
            "test-token",
            "/AnkiCloudSync",
            format!("{}/drive/v3/files", server.uri()),
            format!("{}/upload/drive/v3/files", server.uri()),
        )
    }

    #[tokio::test]
    async fn test_get_or_create_anki_folder_exists() {
        let mock_server = MockServer::start().await;
        let folder_response = json!({
            "files": [{
                "id": "folder-123",
                "name": "AnkiCloudSync"
            }]
        });

        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(folder_response))
            .mount(&mock_server)
            .await;

        let folder_id = backend(&mock_server).get_or_create_anki_folder().await.unwrap();
        assert_eq!(folder_id, "folder-123");
    }

    #[tokio::test]
    async fn test_find_collection_file_exists() {
        let mock_server = MockServer::start().await;
        let file_response = json!({
            "files": [{
                "id": "file-456",
                "name": COLLECTION_FILE_NAME
            }]
        });

        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(file_response))
            .mount(&mock_server)
            .await;

        let file_id = backend(&mock_server)
            .find_collection_file("folder-123")
            .await
            .unwrap();
        assert_eq!(file_id, Some("file-456".to_string()));
    }

    #[tokio::test]
    async fn test_find_collection_file_not_found() {
        let mock_server = MockServer::start().await;
        let empty_response = json!({ "files": [] });

        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_response))
            .mount(&mock_server)
            .await;

        let file_id = backend(&mock_server)
            .find_collection_file("folder-123")
            .await
            .unwrap();
        assert_eq!(file_id, None);
    }

    #[test]
    fn test_q_escape() {
        assert_eq!(q_escape("it's a\\b.png"), "it\\'s a\\\\b.png");
    }

    #[tokio::test]
    async fn test_small_file_uses_single_multipart_request() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload/drive/v3/files"))
            .and(query_param("uploadType", "multipart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "new"})))
            .expect(1)
            .mount(&mock_server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cat.jpg");
        std::fs::write(&src, b"jpeg-bytes").unwrap();

        backend(&mock_server)
            .upload_file("cat.jpg", None, "folder-1", &src)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_existing_file_is_replaced_with_patch() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/upload/drive/v3/files/file-9"))
            .and(query_param("uploadType", "multipart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "file-9"})))
            .expect(1)
            .mount(&mock_server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cat.jpg");
        std::fs::write(&src, b"v2").unwrap();

        backend(&mock_server)
            .upload_file("cat.jpg", Some("file-9"), "folder-1", &src)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_media_get_many_reports_missing_and_downloads_found() {
        let mock_server = MockServer::start().await;
        // Folder lookups (root + media folder) and the name lookup all hit the list endpoint.
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "files": [{"id": "f1", "name": "a.png"}]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/f1"))
            .and(query_param("alt", "media"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PNGDATA".to_vec()))
            .mount(&mock_server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let names = vec!["a.png".to_string(), "b.png".to_string()];
        let missing = backend(&mock_server)
            .media_get_many_async("test-user-get", &names, dir.path())
            .await
            .unwrap();

        assert_eq!(missing, vec!["b.png".to_string()]);
        assert_eq!(std::fs::read(dir.path().join("a.png")).unwrap(), b"PNGDATA");
        assert!(!dir.path().join("a.png.part").exists());
    }

    #[tokio::test]
    async fn test_fetch_removes_stale_wal_and_shm_before_replacing_database() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "files": [{"id": "f1", "name": COLLECTION_FILE_NAME}]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/f1"))
            .and(query_param("alt", "media"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"NEWDB".to_vec()))
            .mount(&mock_server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("collection.anki2");
        std::fs::write(&dest, b"OLDDB").unwrap();
        for suffix in ["-wal", "-shm", "-journal"] {
            std::fs::write(dir.path().join(format!("collection.anki2{suffix}")), b"stale").unwrap();
        }

        backend(&mock_server)
            .fetch_async("test-user-wal", &dest)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"NEWDB");
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!dir.path().join(format!("collection.anki2{suffix}")).exists());
        }
    }

    #[tokio::test]
    async fn test_fetch_without_remote_collection_keeps_local_files() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "files": [] })))
            .mount(&mock_server)
            .await;
        // An empty Drive makes the backend create the sync folder first.
        Mock::given(method("POST"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "new-folder"})))
            .mount(&mock_server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("collection.anki2");
        std::fs::write(dir.path().join("collection.anki2-wal"), b"keep").unwrap();

        // Nothing is downloaded when Drive has no collection, so nothing is replaced either.
        backend(&mock_server)
            .fetch_async("test-user-empty", &dest)
            .await
            .unwrap();

        assert!(dir.path().join("collection.anki2-wal").exists());
    }

    #[tokio::test]
    async fn test_media_delete_many_trashes_found_files() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "files": [{"id": "f1", "name": "a.png"}]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/drive/v3/files/f1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "f1"})))
            .expect(1)
            .mount(&mock_server)
            .await;

        backend(&mock_server)
            .media_delete_many_async("test-user-del", &["a.png".to_string(), "zzz.png".to_string()])
            .await
            .unwrap();
    }
}

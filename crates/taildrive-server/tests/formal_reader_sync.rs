//! Deterministic seeded stateful model-based refinement harness for Reader Sync (SYNC_SPEC rev 3.2).
//!
//! Drives the real TailDrive Axum `/v1/reader` router over TCP + real SQLite `reader.db` + real
//! `LocalFsStorage` (wrapped with controllable free-space reporting for `disk_full` injection),
//! comparing every HTTP response, paginated feed, conflict state, book list, blob hash, and
//! `record_versions` history against an independent pure-Rust reference model after EVERY step.
//!
//! Configuration via env vars:
//! - `FORMAL_SEED`  (u64, default `0x5EED_2026_1128_0001`)
//! - `FORMAL_CASES` (usize total steps across runs, default `200`)

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddr},
    ops::Range,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use rusqlite::Connection;
use serde_json::{Value, json};
use taildrive_core::hash::sha256_hex;
use taildrive_server::{
    App,
    config::Config,
    storage::{ByteStream, LocalFsStorage, StorageBackend, StorageError},
};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

const DEFAULT_SEED: u64 = 0x5EED_2026_1128_0001;
const DEFAULT_TOTAL_STEPS: usize = 200;
const ADMIN_TOKEN: &str = "formal-reader-admin-token";
const FLOOR_BYTES: u64 = 10_000;
const HEALTHY_FREE_BYTES: u64 = 10_000_000;
const FULL_FREE_BYTES: u64 = 5_000;

fn env_seed() -> u64 {
    std::env::var("FORMAL_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SEED)
}

fn env_total_steps() -> usize {
    std::env::var("FORMAL_CASES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TOTAL_STEPS)
}

/// Self-contained deterministic SplitMix64 PRNG so traces replay identically across platforms.
#[derive(Clone, Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, upper: usize) -> usize {
        assert!(upper > 0);
        (self.next_u64() as usize) % upper
    }

    fn chance(&mut self, num: usize, den: usize) -> bool {
        self.range(den) < num
    }
}

// =================================================================================================
// Controllable StorageBackend for disk_full fault injection
// =================================================================================================

struct ControllableStorage {
    local: LocalFsStorage,
    free_bytes: AtomicU64,
}

impl ControllableStorage {
    async fn new(dir: &Path, initial_free: u64) -> Arc<Self> {
        Arc::new(Self {
            local: LocalFsStorage::new(dir).await.expect("local storage"),
            free_bytes: AtomicU64::new(initial_free),
        })
    }

    fn set_free_bytes(&self, bytes: u64) {
        self.free_bytes.store(bytes, Ordering::SeqCst);
    }
}

#[async_trait]
impl StorageBackend for ControllableStorage {
    fn driver_name(&self) -> &'static str {
        self.local.driver_name()
    }

    async fn publish(&self, key: &str, staged: &Path) -> Result<(), StorageError> {
        self.local.publish(key, staged).await
    }

    async fn open(&self, key: &str, range: Range<u64>) -> Result<ByteStream, StorageError> {
        self.local.open(key, range).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.local.delete(key).await
    }

    async fn disk_free_bytes(&self) -> Result<Option<u64>, StorageError> {
        Ok(Some(self.free_bytes.load(Ordering::SeqCst)))
    }
}

// =================================================================================================
// Real TCP Test Server Harness
// =================================================================================================

struct TestServer {
    addr: SocketAddr,
    http: Client,
    dir: TempDir,
    storage: Arc<ControllableStorage>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = ControllableStorage::new(dir.path(), HEALTHY_FREE_BYTES).await;
        let mut config = Config::new(dir.path());
        config.bind = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        config.api_tokens = vec![ADMIN_TOKEN.to_owned()];
        config.free_floor_bytes = FLOOR_BYTES;

        let listener = TcpListener::bind(config.bind).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = App::with_storage(config, storage.clone())
            .await
            .expect("app startup");
        let (shutdown, signal) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            app.serve(listener, async {
                let _ = signal.await;
            })
            .await
        });
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("http client");
        Self {
            addr,
            http,
            dir,
            storage,
            shutdown,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/v1/reader{path}", self.addr)
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.task.await;
    }

    async fn pair(&self, device_id: &str, platform: &str) -> String {
        let kind = if platform == "macos" { "mac" } else { "phone" };
        let resp = self
            .http
            .post(self.url("/v1/pairing/tokens"))
            .bearer_auth(ADMIN_TOKEN)
            .json(&json!({ "kind": kind }))
            .send()
            .await
            .expect("issue token");
        assert_eq!(resp.status(), StatusCode::OK);
        let token = resp.json::<Value>().await.unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned();

        let redeem = self
            .http
            .post(self.url("/v1/pairing/redeem"))
            .json(&json!({
                "token": token,
                "deviceId": device_id,
                "name": format!("{device_id}-name"),
                "platform": platform,
                "appVersion": "1.0.0-formal"
            }))
            .send()
            .await
            .expect("redeem");
        assert_eq!(redeem.status(), StatusCode::OK);
        let body: Value = redeem.json().await.unwrap();
        body["credential"].as_str().unwrap().to_owned()
    }

    async fn send_json(
        &self,
        method: Method,
        cred: &str,
        path: &str,
        body: Option<&Value>,
    ) -> (StatusCode, Vec<u8>, Value) {
        let mut req = self.http.request(method, self.url(path)).bearer_auth(cred);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req.send().await.expect("http send");
        let status = resp.status();
        let bytes = resp.bytes().await.expect("body bytes").to_vec();
        let parsed = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, bytes, parsed)
    }

    async fn send_raw_json(
        &self,
        method: Method,
        cred: &str,
        path: &str,
        raw_body: String,
    ) -> (StatusCode, Vec<u8>, Value) {
        let resp = self
            .http
            .request(method, self.url(path))
            .bearer_auth(cred)
            .header("content-type", "application/json")
            .body(raw_body)
            .send()
            .await
            .expect("http raw send");
        let status = resp.status();
        let bytes = resp.bytes().await.expect("body bytes").to_vec();
        let parsed = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, bytes, parsed)
    }
}

// =================================================================================================
// Independent Pure-Rust SYNC_SPEC rev 3.2 Reference Model
// =================================================================================================

/// Independent canonical JSON serialization (keys sorted by UTF-8 bytes, compact).
fn canonical_json(value: &Value) -> Vec<u8> {
    fn write(v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write(item, out);
                }
                out.push(b']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push(b'{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend(serde_json::to_vec(k).expect("key"));
                    out.push(b':');
                    write(&map[k], out);
                }
                out.push(b'}');
            }
            scalar => out.extend(serde_json::to_vec(scalar).expect("scalar")),
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

/// Independent SYNC_SPEC §6 JSON equality (key order ignored, numbers compared numerically).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if let (Some(xi), Some(yi)) = (x.as_i64(), y.as_i64()) {
                xi == yi
            } else if let (Some(xu), Some(yu)) = (x.as_u64(), y.as_u64()) {
                xu == yu
            } else {
                x.as_f64() == y.as_f64()
            }
        }
        (Value::Array(xs), Value::Array(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(xm), Value::Object(ym)) => {
            xm.len() == ym.len()
                && xm
                    .iter()
                    .all(|(k, v)| ym.get(k).is_some_and(|ov| json_eq(v, ov)))
        }
        _ => a == b,
    }
}

fn opt_json_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => json_eq(x, y),
        (Some(Value::Null), None) | (None, Some(Value::Null)) => true,
        _ => false,
    }
}

/// SYNC_SPEC §6 `same(x, y)`: equal `deleted`, and for live records JSON-equal payloads.
fn same_state(del_a: bool, pay_a: Option<&Value>, del_b: bool, pay_b: Option<&Value>) -> bool {
    del_a == del_b && (del_a || opt_json_eq(pay_a, pay_b))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RecordKey {
    book_id: String,
    kind: String,
    id: String,
}

#[derive(Clone, Debug)]
struct ModelVersion {
    deleted: bool,
    payload: Option<Value>,
    seq: i64,
}

#[derive(Clone, Debug)]
struct ModelRecord {
    version: i64,
    deleted: bool,
    payload: Option<Value>,
    device_id: String,
    change_id: String,
    client_time: Option<String>,
    seq: i64,
    open_conflict_id: Option<String>,
    versions: BTreeMap<i64, ModelVersion>,
}

#[derive(Clone, Debug)]
struct ModelSide {
    change_id: String,
    device_id: String,
    version: Option<i64>,
    deleted: bool,
    payload: Option<Value>,
    client_time: Option<String>,
}

#[derive(Clone, Debug)]
struct ModelConflict {
    conflict_id: String,
    key: RecordKey,
    base_version: i64,
    opened_seq: i64,
    resolved_seq: Option<i64>,
    sides: Vec<ModelSide>,
}

#[derive(Clone, Debug)]
struct ModelBook {
    book_id: String,
    format: String,
    doc_id: Option<String>,
    title: String,
    filename: String,
    current_sha256: String,
    files: Vec<(String, u64)>,
    version: i64,
    seq: i64,
}

#[derive(Clone, Debug, Default)]
struct RefModel {
    seq: i64,
    books: BTreeMap<String, ModelBook>,
    book_files: BTreeMap<String, String>,
    book_doc_ids: BTreeMap<(String, String), String>,
    records: BTreeMap<RecordKey, ModelRecord>,
    conflicts: BTreeMap<String, ModelConflict>,
    batches: BTreeMap<String, (String, Vec<u8>)>,
    applied_changes: BTreeMap<String, (String, Value)>,
    blobs: BTreeMap<String, Vec<u8>>,
}

struct RegisterArgs<'a> {
    device_id: &'a str,
    sha: &'a str,
    size: u64,
    format: &'a str,
    doc_id: Option<&'a str>,
    title: &'a str,
    filename: &'a str,
}

impl RefModel {
    fn open_conflicts(&self) -> Vec<&ModelConflict> {
        let mut list: Vec<&ModelConflict> = self
            .conflicts
            .values()
            .filter(|c| c.resolved_seq.is_none())
            .collect();
        list.sort_by_key(|c| c.opened_seq);
        list
    }

    fn apply_register(
        &mut self,
        args: RegisterArgs<'_>,
        server_resp: &Value,
    ) -> Result<(), String> {
        let RegisterArgs {
            device_id,
            sha,
            size,
            format,
            doc_id,
            title,
            filename,
        } = args;
        let norm_doc_id = doc_id.filter(|d| !d.is_empty()).map(str::to_owned);

        // Rule 1: sha256 already belongs to a book -> existing
        if let Some(book_id) = self.book_files.get(sha).cloned() {
            if server_resp["status"].as_str() != Some("existing") {
                return Err(format!(
                    "register rule 1 expected existing, got {server_resp}"
                ));
            }
            if server_resp["bookId"].as_str() != Some(book_id.as_str()) {
                return Err(format!(
                    "register rule 1 expected bookId {book_id}, got {server_resp}"
                ));
            }
            return Ok(());
        }

        // Rule 2: non-empty docId matching existing live book of same format -> attached
        if let Some(ref did) = norm_doc_id
            && let Some(book_id) = self
                .book_doc_ids
                .get(&(format.to_owned(), did.clone()))
                .cloned()
        {
            if server_resp["status"].as_str() != Some("attached") {
                return Err(format!(
                    "register rule 2 expected attached, got {server_resp}"
                ));
            }
            if server_resp["bookId"].as_str() != Some(book_id.as_str()) {
                return Err(format!(
                    "register rule 2 expected bookId {book_id}, got {server_resp}"
                ));
            }
            self.seq += 1;
            let seq = self.seq;
            let book = self.books.get_mut(&book_id).expect("existing book");
            book.files.push((sha.to_owned(), size));
            book.current_sha256 = sha.to_owned();
            book.version += 1;
            book.seq = seq;
            let version = book.version;
            self.book_files.insert(sha.to_owned(), book_id.clone());

            let key = RecordKey {
                book_id: book_id.clone(),
                kind: "book".to_owned(),
                id: book_id.clone(),
            };
            let payload = server_resp["book"]["payload"].clone();
            if payload["currentSha256"].as_str() != Some(sha) {
                return Err(format!(
                    "attached book payload wrong currentSha256: {payload}"
                ));
            }
            let rec = self.records.get_mut(&key).expect("book record");
            rec.version = version;
            rec.payload = Some(payload.clone());
            rec.device_id = device_id.to_owned();
            rec.change_id = format!("c_reg_{seq}");
            rec.seq = seq;
            rec.versions.insert(
                version,
                ModelVersion {
                    deleted: false,
                    payload: Some(payload),
                    seq,
                },
            );
            return Ok(());
        }

        // Rule 3: created
        let book_id = format!("sha256:{sha}");
        if server_resp["status"].as_str() != Some("created") {
            return Err(format!(
                "register rule 3 expected created, got {server_resp}"
            ));
        }
        if server_resp["bookId"].as_str() != Some(book_id.as_str()) {
            return Err(format!(
                "register rule 3 expected bookId {book_id}, got {server_resp}"
            ));
        }
        self.seq += 1;
        let seq = self.seq;
        let payload = server_resp["book"]["payload"].clone();
        if payload["currentSha256"].as_str() != Some(sha)
            || payload["format"].as_str() != Some(format)
            || payload["title"].as_str() != Some(title)
            || payload["filename"].as_str() != Some(filename)
        {
            return Err(format!("created book payload mismatch: {payload}"));
        }
        self.books.insert(
            book_id.clone(),
            ModelBook {
                book_id: book_id.clone(),
                format: format.to_owned(),
                doc_id: norm_doc_id.clone(),
                title: title.to_owned(),
                filename: filename.to_owned(),
                current_sha256: sha.to_owned(),
                files: vec![(sha.to_owned(), size)],
                version: 1,
                seq,
            },
        );
        self.book_files.insert(sha.to_owned(), book_id.clone());
        if let Some(did) = norm_doc_id {
            self.book_doc_ids
                .entry((format.to_owned(), did))
                .or_insert_with(|| book_id.clone());
        }
        let mut versions = BTreeMap::new();
        versions.insert(
            1,
            ModelVersion {
                deleted: false,
                payload: Some(payload.clone()),
                seq,
            },
        );
        self.records.insert(
            RecordKey {
                book_id: book_id.clone(),
                kind: "book".to_owned(),
                id: book_id,
            },
            ModelRecord {
                version: 1,
                deleted: false,
                payload: Some(payload),
                device_id: device_id.to_owned(),
                change_id: format!("c_reg_{seq}"),
                client_time: None,
                seq,
                open_conflict_id: None,
                versions,
            },
        );
        Ok(())
    }

    /// Simulates a `POST /v1/reader/v1/changes` batch against the independent reference model
    /// and verifies the server's HTTP status, response bytes, and every per-change result!
    fn apply_push_batch(
        &mut self,
        device_id: &str,
        batch_body: &Value,
        status: StatusCode,
        resp_bytes: &[u8],
        resp_json: &Value,
    ) -> Result<(), String> {
        let batch_id = batch_body["batchId"].as_str().unwrap();
        let req_hash = sha256_hex(canonical_json(batch_body));

        // Batch idempotency check (§5.3)
        if let Some((prior_hash, prior_bytes)) = self.batches.get(batch_id) {
            if prior_hash != &req_hash {
                if status != StatusCode::UNPROCESSABLE_ENTITY
                    || resp_json["error"]["code"].as_str() != Some("change_id_reused")
                {
                    return Err(format!(
                        "batchId reuse with different body expected 422 change_id_reused, got {status} {resp_json}"
                    ));
                }
                return Ok(());
            }
            if status != StatusCode::OK {
                return Err(format!(
                    "batch replay expected 200, got {status} {resp_json}"
                ));
            }
            if resp_bytes != prior_bytes.as_slice() {
                return Err(format!(
                    "batch replay bytes differed!\nexpected: {}\nactual:   {}",
                    String::from_utf8_lossy(prior_bytes),
                    String::from_utf8_lossy(resp_bytes)
                ));
            }
            return Ok(());
        }

        if status != StatusCode::OK {
            return Err(format!(
                "valid batch push expected 200, got {status} {resp_json}"
            ));
        }

        let changes = batch_body["changes"].as_array().unwrap();
        let server_results = resp_json["results"]
            .as_array()
            .ok_or_else(|| format!("missing results array: {resp_json}"))?;
        if server_results.len() != changes.len() {
            return Err(format!(
                "results len mismatch: expected {}, got {}",
                changes.len(),
                server_results.len()
            ));
        }

        for (chg, actual) in changes.iter().zip(server_results.iter()) {
            self.apply_one_change(device_id, chg, actual)?;
        }

        if resp_json["seq"].as_i64() != Some(self.seq) {
            return Err(format!(
                "batch response seq mismatch: expected {}, got {}",
                self.seq, resp_json["seq"]
            ));
        }
        if resp_json["batchId"].as_str() != Some(batch_id) {
            return Err(format!("batchId mismatch in response: {resp_json}"));
        }

        self.batches
            .insert(batch_id.to_owned(), (req_hash, resp_bytes.to_vec()));
        Ok(())
    }

    fn apply_one_change(
        &mut self,
        device_id: &str,
        chg: &Value,
        actual: &Value,
    ) -> Result<(), String> {
        let change_id = chg["changeId"].as_str().unwrap().to_owned();
        let book_id = chg["bookId"].as_str().unwrap().to_owned();
        let kind = chg["kind"].as_str().unwrap().to_owned();
        let id = chg["id"].as_str().unwrap().to_owned();
        let base_version = chg["baseVersion"].as_i64().unwrap();
        let deleted = chg["deleted"].as_bool().unwrap();
        let client_time = chg
            .get("clientTime")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let payload_opt = match &chg["payload"] {
            Value::Null => None,
            other => Some(other.clone()),
        };
        let chg_hash = sha256_hex(canonical_json(chg));

        // 1. Per-change idempotency (§5.3)
        if let Some((prior_hash, prior_result)) = self.applied_changes.get(&change_id) {
            if prior_hash == &chg_hash {
                if !json_eq(actual, prior_result) {
                    return Err(format!(
                        "replayed changeId {change_id} returned {actual}, expected {prior_result}"
                    ));
                }
            } else if actual["status"].as_str() != Some("rejected")
                || actual["error"]["code"].as_str() != Some("change_id_reused")
            {
                return Err(format!(
                    "reused changeId {change_id} with different body expected rejected change_id_reused, got {actual}"
                ));
            }
            return Ok(());
        }

        // 2. Book existence check
        if !self.books.contains_key(&book_id) {
            if actual["status"].as_str() != Some("rejected")
                || actual["error"]["code"].as_str() != Some("unknown_book")
            {
                return Err(format!(
                    "unknown book change expected rejected unknown_book, got {actual}"
                ));
            }
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        }

        let key = RecordKey {
            book_id: book_id.clone(),
            kind: kind.clone(),
            id: id.clone(),
        };

        // 3. Position rules (§3.4)
        if kind == "position" {
            if deleted {
                if actual["status"].as_str() != Some("rejected")
                    || actual["error"]["code"].as_str() != Some("bad_request")
                {
                    return Err(format!(
                        "position delete expected rejected bad_request, got {actual}"
                    ));
                }
                self.applied_changes
                    .insert(change_id, (chg_hash, actual.clone()));
                return Ok(());
            }
            let expected_pos_id = format!("p_{book_id}_{device_id}");
            let payload_owned = payload_opt.as_ref().is_some_and(|p| {
                p.get("bookId").and_then(Value::as_str) == Some(book_id.as_str())
                    && p.get("deviceId").and_then(Value::as_str) == Some(device_id)
            });
            if id != expected_pos_id || !payload_owned {
                if actual["status"].as_str() != Some("rejected")
                    || actual["error"]["code"].as_str() != Some("not_owner")
                {
                    return Err(format!(
                        "foreign position expected rejected not_owner, got {actual}"
                    ));
                }
                self.applied_changes
                    .insert(change_id, (chg_hash, actual.clone()));
                return Ok(());
            }

            if let Some(head) = self.records.get(&key)
                && !head.deleted
                && opt_json_eq(head.payload.as_ref(), payload_opt.as_ref())
            {
                let expected =
                    json!({"changeId": change_id, "status": "noop", "version": head.version});
                if !json_eq(actual, &expected) {
                    return Err(format!(
                        "position identical expected {expected}, got {actual}"
                    ));
                }
                self.applied_changes
                    .insert(change_id, (chg_hash, actual.clone()));
                return Ok(());
            }

            // Create or fast-forward position (baseVersion ignored!)
            self.seq += 1;
            let seq = self.seq;
            let new_ver = self.records.get(&key).map_or(1, |h| h.version + 1);
            let expected =
                json!({"changeId": change_id, "status": "applied", "version": new_ver, "seq": seq});
            if !json_eq(actual, &expected) {
                return Err(format!("position write expected {expected}, got {actual}"));
            }
            let entry = self.records.entry(key).or_insert_with(|| ModelRecord {
                version: 0,
                deleted: false,
                payload: None,
                device_id: String::new(),
                change_id: String::new(),
                client_time: None,
                seq: 0,
                open_conflict_id: None,
                versions: BTreeMap::new(),
            });
            entry.version = new_ver;
            entry.deleted = false;
            entry.payload = payload_opt.clone();
            entry.device_id = device_id.to_owned();
            entry.change_id = change_id.clone();
            entry.client_time = client_time;
            entry.seq = seq;
            entry.versions.insert(
                new_ver,
                ModelVersion {
                    deleted: false,
                    payload: payload_opt,
                    seq,
                },
            );
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        }

        // 4. General record merge rules (§6)
        let Some(head_snapshot) = self.records.get(&key).cloned() else {
            if base_version == 0 {
                self.seq += 1;
                let seq = self.seq;
                let expected =
                    json!({"changeId": change_id, "status": "applied", "version": 1, "seq": seq});
                if !json_eq(actual, &expected) {
                    return Err(format!("create expected {expected}, got {actual}"));
                }
                let mut versions = BTreeMap::new();
                versions.insert(
                    1,
                    ModelVersion {
                        deleted,
                        payload: payload_opt.clone(),
                        seq,
                    },
                );
                self.records.insert(
                    key,
                    ModelRecord {
                        version: 1,
                        deleted,
                        payload: payload_opt,
                        device_id: device_id.to_owned(),
                        change_id: change_id.clone(),
                        client_time,
                        seq,
                        open_conflict_id: None,
                        versions,
                    },
                );
            } else if actual["status"].as_str() != Some("rejected")
                || actual["error"]["code"].as_str() != Some("bad_base")
            {
                return Err(format!(
                    "absent record with base>0 expected rejected bad_base, got {actual}"
                ));
            }
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        };

        // Identical to head -> noop (regardless of baseVersion!)
        if same_state(
            head_snapshot.deleted,
            head_snapshot.payload.as_ref(),
            deleted,
            payload_opt.as_ref(),
        ) {
            let expected =
                json!({"changeId": change_id, "status": "noop", "version": head_snapshot.version});
            if !json_eq(actual, &expected) {
                return Err(format!("noop head expected {expected}, got {actual}"));
            }
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        }

        // Future base -> bad_base
        if base_version > head_snapshot.version {
            if actual["status"].as_str() != Some("rejected")
                || actual["error"]["code"].as_str() != Some("bad_base")
            {
                return Err(format!(
                    "future base expected rejected bad_base, got {actual}"
                ));
            }
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        }

        // Exact base == head.version -> fast-forward (open conflict stays open)
        if base_version == head_snapshot.version {
            self.seq += 1;
            let seq = self.seq;
            let new_ver = head_snapshot.version + 1;
            let expected =
                json!({"changeId": change_id, "status": "applied", "version": new_ver, "seq": seq});
            if !json_eq(actual, &expected) {
                return Err(format!("fast-forward expected {expected}, got {actual}"));
            }
            let stored_payload = if deleted {
                payload_opt.or(head_snapshot.payload)
            } else {
                payload_opt
            };
            let rec = self.records.get_mut(&key).unwrap();
            rec.version = new_ver;
            rec.deleted = deleted;
            rec.payload = stored_payload.clone();
            rec.device_id = device_id.to_owned();
            rec.change_id = change_id.clone();
            rec.client_time = client_time;
            rec.seq = seq;
            rec.versions.insert(
                new_ver,
                ModelVersion {
                    deleted,
                    payload: stored_payload,
                    seq,
                },
            );
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        }

        // base_version < head_snapshot.version -> check open conflict sides
        if let Some(ref cid) = head_snapshot.open_conflict_id {
            let conflict = self.conflicts.get(cid).expect("open conflict exists");
            if conflict
                .sides
                .iter()
                .any(|s| same_state(s.deleted, s.payload.as_ref(), deleted, payload_opt.as_ref()))
            {
                let expected = json!({
                    "changeId": change_id,
                    "status": "noop",
                    "version": head_snapshot.version,
                    "conflictId": cid,
                });
                if !json_eq(actual, &expected) {
                    return Err(format!("noop conflict expected {expected}, got {actual}"));
                }
                self.applied_changes
                    .insert(change_id, (chg_hash, actual.clone()));
                return Ok(());
            }

            // Append new side to existing open conflict
            self.seq += 1;
            let seq = self.seq;
            let expected = json!({
                "changeId": change_id,
                "status": "conflict",
                "version": head_snapshot.version,
                "conflictId": cid,
                "seq": seq,
            });
            if !json_eq(actual, &expected) {
                return Err(format!("append conflict expected {expected}, got {actual}"));
            }
            let conflict = self.conflicts.get_mut(cid).unwrap();
            conflict.sides.push(ModelSide {
                change_id: change_id.clone(),
                device_id: device_id.to_owned(),
                version: None,
                deleted,
                payload: payload_opt,
                client_time,
            });
            self.records.get_mut(&key).unwrap().seq = seq;
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        }

        // Open new conflict (base_version == 0 or stored in versions)
        if base_version > 0 && !head_snapshot.versions.contains_key(&base_version) {
            if actual["status"].as_str() != Some("rejected")
                || actual["error"]["code"].as_str() != Some("bad_base")
            {
                return Err(format!(
                    "missing base version expected rejected bad_base, got {actual}"
                ));
            }
            self.applied_changes
                .insert(change_id, (chg_hash, actual.clone()));
            return Ok(());
        }

        self.seq += 1;
        let seq = self.seq;
        let conflict_id = actual["conflictId"]
            .as_str()
            .ok_or_else(|| format!("open conflict missing conflictId: {actual}"))?
            .to_owned();
        if self.conflicts.contains_key(&conflict_id) {
            return Err(format!("server reused existing conflictId {conflict_id}"));
        }
        let expected = json!({
            "changeId": change_id,
            "status": "conflict",
            "version": head_snapshot.version,
            "conflictId": conflict_id,
            "seq": seq,
        });
        if !json_eq(actual, &expected) {
            return Err(format!("open conflict expected {expected}, got {actual}"));
        }
        let sides = vec![
            ModelSide {
                change_id: head_snapshot.change_id.clone(),
                device_id: head_snapshot.device_id.clone(),
                version: Some(head_snapshot.version),
                deleted: head_snapshot.deleted,
                payload: head_snapshot.payload.clone(),
                client_time: head_snapshot.client_time.clone(),
            },
            ModelSide {
                change_id: change_id.clone(),
                device_id: device_id.to_owned(),
                version: None,
                deleted,
                payload: payload_opt,
                client_time,
            },
        ];
        self.conflicts.insert(
            conflict_id.clone(),
            ModelConflict {
                conflict_id: conflict_id.clone(),
                key: key.clone(),
                base_version,
                opened_seq: seq,
                resolved_seq: None,
                sides,
            },
        );
        let rec = self.records.get_mut(&key).unwrap();
        rec.seq = seq;
        rec.open_conflict_id = Some(conflict_id);
        self.applied_changes
            .insert(change_id, (chg_hash, actual.clone()));
        Ok(())
    }

    fn apply_resolve(
        &mut self,
        device_id: &str,
        conflict_id: &str,
        body: &Value,
        status: StatusCode,
        resp_bytes: &[u8],
        resp_json: &Value,
    ) -> Result<(), String> {
        let change_id = body["changeId"].as_str().unwrap().to_owned();
        let base_version = body["baseVersion"].as_i64().unwrap();
        let deleted = body["deleted"].as_bool().unwrap();
        let payload_opt = match &body["payload"] {
            Value::Null => None,
            other => Some(other.clone()),
        };
        let req_hash = sha256_hex(canonical_json(body));

        // Idempotent replay / reuse check on changeId
        if let Some((prior_hash, prior_result)) = self.applied_changes.get(&change_id) {
            if prior_hash != &req_hash {
                if status != StatusCode::UNPROCESSABLE_ENTITY
                    || resp_json["error"]["code"].as_str() != Some("change_id_reused")
                {
                    return Err(format!(
                        "resolve changeId reuse expected 422 change_id_reused, got {status} {resp_json}"
                    ));
                }
                return Ok(());
            }
            if status != StatusCode::OK {
                return Err(format!(
                    "resolve replay expected 200, got {status} {resp_json}"
                ));
            }
            if !json_eq(resp_json, prior_result) {
                return Err(format!(
                    "resolve replay mismatch: expected {prior_result}, got {resp_json}"
                ));
            }
            if resp_bytes != canonical_json(prior_result).as_slice() {
                return Err("resolve replay not byte-for-byte identical".to_owned());
            }
            return Ok(());
        }

        let Some(conflict) = self.conflicts.get(conflict_id).cloned() else {
            if status != StatusCode::NOT_FOUND {
                return Err(format!("unknown conflict expected 404, got {status}"));
            }
            return Ok(());
        };
        if conflict.resolved_seq.is_some() {
            if status != StatusCode::CONFLICT
                || resp_json["error"]["code"].as_str() != Some("already_resolved")
            {
                return Err(format!(
                    "already resolved expected 409 already_resolved, got {status} {resp_json}"
                ));
            }
            return Ok(());
        }
        let head = self.records.get(&conflict.key).cloned().expect("head");
        if base_version != head.version {
            if status != StatusCode::CONFLICT
                || resp_json["error"]["code"].as_str() != Some("stale_base")
            {
                return Err(format!(
                    "stale resolve expected 409 stale_base, got {status} {resp_json}"
                ));
            }
            return Ok(());
        }

        if status != StatusCode::OK {
            return Err(format!(
                "valid resolve expected 200, got {status} {resp_json}"
            ));
        }
        self.seq += 1;
        let seq = self.seq;
        let new_ver = head.version + 1;
        let expected = json!({ "seq": seq, "status": "applied", "version": new_ver });
        if !json_eq(resp_json, &expected) {
            return Err(format!("resolve expected {expected}, got {resp_json}"));
        }

        let stored_payload = if deleted {
            payload_opt.or(head.payload)
        } else {
            payload_opt
        };
        let rec = self.records.get_mut(&conflict.key).unwrap();
        rec.version = new_ver;
        rec.deleted = deleted;
        rec.payload = stored_payload.clone();
        rec.device_id = device_id.to_owned();
        rec.change_id = change_id.clone();
        rec.client_time = None;
        rec.seq = seq;
        rec.open_conflict_id = None;
        rec.versions.insert(
            new_ver,
            ModelVersion {
                deleted,
                payload: stored_payload,
                seq,
            },
        );
        self.conflicts.get_mut(conflict_id).unwrap().resolved_seq = Some(seq);
        self.applied_changes.insert(change_id, (req_hash, expected));
        Ok(())
    }
}

// =================================================================================================
// Server Snapshot & Invariant Oracle (SYNC_SPEC §6 & §12.3)
// =================================================================================================

#[derive(Clone, Debug)]
struct DbVersionRow {
    version: i64,
    deleted: bool,
    payload: Option<Value>,
    seq: i64,
}

#[derive(Clone, Debug)]
struct ServerSnapshot {
    health_seq: i64,
    feed_records: Vec<Value>,
    open_conflicts: Vec<Value>,
    books: Vec<Value>,
    db_versions: BTreeMap<RecordKey, Vec<DbVersionRow>>,
    verified_blobs: BTreeMap<String, String>,
}

fn verify_conflict_json(
    expected: &ModelConflict,
    record: &ModelRecord,
    actual: &Value,
) -> Result<(), String> {
    if actual["conflictId"].as_str() != Some(expected.conflict_id.as_str()) {
        return Err(format!(
            "conflictId mismatch: expected {}, got {actual}",
            expected.conflict_id
        ));
    }
    if actual["bookId"].as_str() != Some(expected.key.book_id.as_str())
        || actual["kind"].as_str() != Some(expected.key.kind.as_str())
        || actual["id"].as_str() != Some(expected.key.id.as_str())
    {
        return Err(format!(
            "conflict key mismatch: expected {:?}, got {actual}",
            expected.key
        ));
    }
    if actual["baseVersion"].as_i64() != Some(expected.base_version)
        || actual["openedSeq"].as_i64() != Some(expected.opened_seq)
    {
        return Err(format!("conflict baseVersion/openedSeq mismatch: {actual}"));
    }
    if expected.base_version == 0 {
        if !actual["base"].is_null() {
            return Err(format!("baseVersion=0 must have base=null, got {actual}"));
        }
    } else {
        let base_ver = record
            .versions
            .get(&expected.base_version)
            .ok_or_else(|| format!("missing model base version {}", expected.base_version))?;
        let base_json = &actual["base"];
        if base_json["deleted"].as_bool() != Some(base_ver.deleted) {
            return Err(format!("conflict base.deleted mismatch: {actual}"));
        }
        let actual_base_pay = match &base_json["payload"] {
            Value::Null => None,
            v => Some(v),
        };
        if !opt_json_eq(base_ver.payload.as_ref(), actual_base_pay) {
            return Err(format!("conflict base.payload mismatch: {actual}"));
        }
    }

    let sides = actual["sides"]
        .as_array()
        .ok_or_else(|| format!("conflict missing sides array: {actual}"))?;
    if sides.len() < 2 {
        return Err(format!("open conflict has fewer than 2 sides: {actual}"));
    }
    if sides.len() != expected.sides.len() {
        return Err(format!(
            "conflict sides len mismatch: expected {}, got {} ({actual})",
            expected.sides.len(),
            sides.len()
        ));
    }
    for (i, (exp, got)) in expected.sides.iter().zip(sides.iter()).enumerate() {
        if got["changeId"].as_str() != Some(exp.change_id.as_str())
            || got["deviceId"].as_str() != Some(exp.device_id.as_str())
            || got["version"].as_i64() != exp.version
            || got["deleted"].as_bool() != Some(exp.deleted)
            || got["clientTime"].as_str() != exp.client_time.as_deref()
        {
            return Err(format!(
                "conflict side[{i}] metadata mismatch: expected {exp:?}, got {got}"
            ));
        }
        let got_pay = match &got["payload"] {
            Value::Null => None,
            v => Some(v),
        };
        if !opt_json_eq(exp.payload.as_ref(), got_pay) {
            return Err(format!(
                "conflict side[{i}] payload mismatch: expected {exp:?}, got {got}"
            ));
        }
    }

    // §6 / §12.3 invariant: NO two sides in an open conflict may be `same`!
    for i in 0..sides.len() {
        for j in (i + 1)..sides.len() {
            let del_i = sides[i]["deleted"].as_bool().unwrap_or(false);
            let pay_i = match &sides[i]["payload"] {
                Value::Null => None,
                v => Some(v),
            };
            let del_j = sides[j]["deleted"].as_bool().unwrap_or(false);
            let pay_j = match &sides[j]["payload"] {
                Value::Null => None,
                v => Some(v),
            };
            if same_state(del_i, pay_i, del_j, pay_j) {
                return Err(format!(
                    "SYNC_SPEC §6 violation: conflict {} has duplicate/same sides [{i}] and [{j}]: {actual}",
                    expected.conflict_id
                ));
            }
        }
    }

    Ok(())
}

/// Independent SYNC_SPEC §6 & §12.3 invariant oracle checked after every step (and tested by canaries).
fn verify_invariants(model: &RefModel, snap: &ServerSnapshot) -> Result<(), String> {
    // 1. Global seq matches model
    if snap.health_seq != model.seq {
        return Err(format!(
            "health seq mismatch: model={}, server={}",
            model.seq, snap.health_seq
        ));
    }

    // 2. Feed records have unique, strictly increasing seq in 1..=health_seq and match model 1-to-1
    let mut prev_seq = 0_i64;
    let mut seen_seqs = BTreeSet::new();
    let mut seen_keys = BTreeSet::new();
    for env in &snap.feed_records {
        let seq = env["seq"]
            .as_i64()
            .ok_or_else(|| format!("feed entry missing seq: {env}"))?;
        if seq <= 0 || seq > snap.health_seq {
            return Err(format!("seq {seq} out of bounds 1..={}", snap.health_seq));
        }
        if seq <= prev_seq {
            return Err(format!(
                "feed seq not strictly increasing: {prev_seq} -> {seq}"
            ));
        }
        if !seen_seqs.insert(seq) {
            return Err(format!("duplicate seq {seq} in feed"));
        }
        prev_seq = seq;

        let key = RecordKey {
            book_id: env["bookId"].as_str().unwrap_or_default().to_owned(),
            kind: env["kind"].as_str().unwrap_or_default().to_owned(),
            id: env["id"].as_str().unwrap_or_default().to_owned(),
        };
        if !seen_keys.insert(key.clone()) {
            return Err(format!("duplicate record key {key:?} in feed"));
        }

        let Some(exp) = model.records.get(&key) else {
            return Err(format!("unexpected record {key:?} in server feed: {env}"));
        };

        if env["version"].as_i64() != Some(exp.version)
            || env["deleted"].as_bool() != Some(exp.deleted)
            || env["deviceId"].as_str() != Some(exp.device_id.as_str())
            || env["changeId"].as_str() != Some(exp.change_id.as_str())
            || env["seq"].as_i64() != Some(exp.seq)
        {
            return Err(format!(
                "head mismatch for {key:?}: model=(v={}, del={}, dev={}, cid={}, seq={}), server={env}",
                exp.version, exp.deleted, exp.device_id, exp.change_id, exp.seq
            ));
        }
        let got_pay = match &env["payload"] {
            Value::Null => None,
            v => Some(v),
        };
        if !opt_json_eq(exp.payload.as_ref(), got_pay) {
            return Err(format!(
                "payload mismatch for {key:?}: model={:?}, server={env}",
                exp.payload
            ));
        }

        // Position invariants (§3.4)
        if key.kind == "position" {
            if !env["conflict"].is_null() {
                return Err(format!("position record must never have a conflict: {env}"));
            }
            if env["deleted"].as_bool() != Some(false) {
                return Err(format!("position record must never be deleted: {env}"));
            }
            let expected_id = format!("p_{}_{}", key.book_id, exp.device_id);
            if key.id != expected_id {
                return Err(format!("position id ownership broken: {env}"));
            }
            if env["payload"]["deviceId"].as_str() != Some(exp.device_id.as_str())
                || env["payload"]["bookId"].as_str() != Some(key.book_id.as_str())
            {
                return Err(format!("position payload ownership broken: {env}"));
            }
        }

        // Embedded conflict check
        match &exp.open_conflict_id {
            None => {
                if !env["conflict"].is_null() {
                    return Err(format!("unexpected open conflict on {key:?}: {env}"));
                }
            }
            Some(cid) => {
                let model_cf = model.conflicts.get(cid).expect("model conflict");
                verify_conflict_json(model_cf, exp, &env["conflict"])?;
            }
        }
    }

    if seen_keys.len() != model.records.len() {
        return Err(format!(
            "record count mismatch: feed has {}, model has {}",
            seen_keys.len(),
            model.records.len()
        ));
    }

    // 3. `/v1/conflicts` matches model open conflicts in openedSeq order
    let expected_open = model.open_conflicts();
    if snap.open_conflicts.len() != expected_open.len() {
        return Err(format!(
            "open conflict count mismatch: server={}, model={}",
            snap.open_conflicts.len(),
            expected_open.len()
        ));
    }
    for (exp_cf, got_cf) in expected_open.iter().zip(snap.open_conflicts.iter()) {
        let rec = model.records.get(&exp_cf.key).expect("conflict record");
        verify_conflict_json(exp_cf, rec, got_cf)?;
    }

    // 4. `/v1/books` matches model books and every `hasBlob` blob re-hashes cleanly
    if snap.books.len() != model.books.len() {
        return Err(format!(
            "books count mismatch: server={}, model={}",
            snap.books.len(),
            model.books.len()
        ));
    }
    let mut expected_books: Vec<&ModelBook> = model.books.values().collect();
    expected_books.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.book_id.cmp(&b.book_id)));
    for (exp_b, got_b) in expected_books.iter().zip(snap.books.iter()) {
        let got_doc_id = got_b.get("docId").and_then(Value::as_str);
        if got_b["bookId"].as_str() != Some(exp_b.book_id.as_str())
            || got_b["format"].as_str() != Some(exp_b.format.as_str())
            || got_doc_id != exp_b.doc_id.as_deref()
            || got_b["title"].as_str() != Some(exp_b.title.as_str())
            || got_b["filename"].as_str() != Some(exp_b.filename.as_str())
            || got_b["sha256"].as_str() != Some(exp_b.current_sha256.as_str())
            || got_b["version"].as_i64() != Some(exp_b.version)
            || got_b["seq"].as_i64() != Some(exp_b.seq)
            || got_b["hasBlob"].as_bool() != Some(model.blobs.contains_key(&exp_b.current_sha256))
        {
            return Err(format!(
                "book summary mismatch: expected {exp_b:?}, got {got_b}"
            ));
        }
        let files = got_b["files"]
            .as_array()
            .ok_or_else(|| format!("book missing files: {got_b}"))?;
        if files.len() != exp_b.files.len() {
            return Err(format!("book files len mismatch: {got_b}"));
        }
        for ((sha, size), f) in exp_b.files.iter().zip(files.iter()) {
            let expected_has = model.blobs.contains_key(sha);
            if f["sha256"].as_str() != Some(sha.as_str())
                || f["size"].as_u64() != Some(*size)
                || f["hasBlob"].as_bool() != Some(expected_has)
            {
                return Err(format!("book file entry mismatch: {f}"));
            }
            if expected_has {
                match snap.verified_blobs.get(sha) {
                    Some(actual_sha) if actual_sha == sha => {}
                    other => {
                        return Err(format!(
                            "blob {sha} hash verification failed (got {other:?})"
                        ));
                    }
                }
            }
        }
    }

    // 5. `record_versions` completeness: every record at version V has exact 1..=V with increasing seq
    if snap.db_versions.len() != model.records.len() {
        return Err(format!(
            "record_versions key count mismatch: db={}, model={}",
            snap.db_versions.len(),
            model.records.len()
        ));
    }
    for (key, rec) in &model.records {
        let rows = snap
            .db_versions
            .get(key)
            .ok_or_else(|| format!("missing record_versions for {key:?}"))?;
        if rows.len() != rec.version as usize {
            return Err(format!(
                "record_versions gap for {key:?}: head version={}, stored rows={}",
                rec.version,
                rows.len()
            ));
        }
        let mut last_ver_seq = 0_i64;
        for (idx, row) in rows.iter().enumerate() {
            let expected_ver = (idx as i64) + 1;
            if row.version != expected_ver {
                return Err(format!(
                    "record_versions non-contiguous version for {key:?}: expected {expected_ver}, got {}",
                    row.version
                ));
            }
            if row.seq <= last_ver_seq {
                return Err(format!(
                    "record_versions seq not increasing for {key:?}: {last_ver_seq} -> {}",
                    row.seq
                ));
            }
            last_ver_seq = row.seq;
            let mv = rec
                .versions
                .get(&expected_ver)
                .ok_or_else(|| format!("model missing version {expected_ver} for {key:?}"))?;
            if row.deleted != mv.deleted
                || row.seq != mv.seq
                || !opt_json_eq(mv.payload.as_ref(), row.payload.as_ref())
            {
                return Err(format!(
                    "record_versions content mismatch for {key:?} v{expected_ver}"
                ));
            }
        }
    }

    Ok(())
}

async fn capture_snapshot(
    srv: &TestServer,
    cred: &str,
    model: &RefModel,
    page_limit: usize,
) -> ServerSnapshot {
    let (h_status, _, health) = srv.send_json(Method::GET, cred, "/v1/health", None).await;
    assert_eq!(h_status, StatusCode::OK);
    let health_seq = health["seq"].as_i64().unwrap();

    // Paginated pull across the entire feed with small `limit` to verify cursor & paging every step!
    let mut since = 0_i64;
    let mut feed_records = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= 500, "infinite pagination loop detected");
        let (status, _, page) = srv
            .send_json(
                Method::GET,
                cred,
                &format!("/v1/changes?since={since}&limit={page_limit}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "pull failed: {page}");
        let items = page["changes"].as_array().unwrap();
        assert!(items.len() <= page_limit);
        for item in items {
            assert!(
                item["seq"].as_i64().unwrap() > since,
                "returned seq <= since ({since}): {item}"
            );
            feed_records.push(item.clone());
        }
        let next_cursor = page["cursor"].as_i64().unwrap();
        if let Some(last) = items.last() {
            assert_eq!(next_cursor, last["seq"].as_i64().unwrap());
        } else {
            assert_eq!(next_cursor, since);
        }
        since = next_cursor;
        if !page["hasMore"].as_bool().unwrap() {
            break;
        }
    }

    let (c_status, _, conflicts_body) = srv
        .send_json(Method::GET, cred, "/v1/conflicts", None)
        .await;
    assert_eq!(c_status, StatusCode::OK);
    let open_conflicts = conflicts_body["conflicts"].as_array().unwrap().clone();

    let (b_status, _, books_body) = srv.send_json(Method::GET, cred, "/v1/books", None).await;
    assert_eq!(b_status, StatusCode::OK);
    let books = books_body["books"].as_array().unwrap().clone();

    // Verify every blob claimed present downloads cleanly and matches its sha256.
    let mut verified_blobs = BTreeMap::new();
    for sha in model.blobs.keys() {
        let resp = srv
            .http
            .get(srv.url(&format!("/v1/blobs/{sha}")))
            .bearer_auth(cred)
            .send()
            .await
            .expect("get blob");
        assert_eq!(resp.status(), StatusCode::OK, "expected stored blob {sha}");
        let body = resp.bytes().await.expect("blob bytes");
        verified_blobs.insert(sha.clone(), sha256_hex(&body));
    }

    // Read-only SQLite inspection of `record_versions`.
    let db_path = srv.dir.path().join("reader.db");
    let db_versions = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path).expect("open reader.db");
        let mut stmt = conn
            .prepare(
                "SELECT book_id, kind, id, version, deleted, payload, seq
                 FROM record_versions ORDER BY book_id, kind, id, version",
            )
            .expect("prepare");
        let mut map: BTreeMap<RecordKey, Vec<DbVersionRow>> = BTreeMap::new();
        let rows = stmt
            .query_map([], |row| {
                let payload_str: Option<String> = row.get(5)?;
                let payload = payload_str.map(|s| serde_json::from_str::<Value>(&s).unwrap());
                Ok((
                    RecordKey {
                        book_id: row.get(0)?,
                        kind: row.get(1)?,
                        id: row.get(2)?,
                    },
                    DbVersionRow {
                        version: row.get(3)?,
                        deleted: row.get(4)?,
                        payload,
                        seq: row.get(6)?,
                    },
                ))
            })
            .expect("query");
        for row in rows {
            let (k, v) = row.expect("row");
            map.entry(k).or_default().push(v);
        }
        map
    })
    .await
    .expect("sqlite read");

    ServerSnapshot {
        health_seq,
        feed_records,
        open_conflicts,
        books,
        db_versions,
        verified_blobs,
    }
}

// =================================================================================================
// Stateful Randomized Scenario Driver
// =================================================================================================

#[derive(Default, Debug)]
struct CoverageStats {
    register_created: usize,
    register_existing: usize,
    register_attached: usize,
    push_applied: usize,
    push_noop_head: usize,
    push_noop_conflict: usize,
    push_conflict_open: usize,
    push_conflict_append: usize,
    push_rejected_bad_base: usize,
    push_rejected_unknown_book: usize,
    push_rejected_not_owner: usize,
    push_rejected_pos_delete: usize,
    push_rejected_change_id_reused: usize,
    push_change_id_replayed: usize,
    batch_replayed_ok: usize,
    batch_reused_422: usize,
    resolve_applied: usize,
    resolve_stale_409: usize,
    resolve_already_409: usize,
    resolve_replayed_ok: usize,
    resolve_reused_422: usize,
    blob_created: usize,
    blob_deduped: usize,
    blob_disk_full_507: usize,
    pull_mid_cursor: usize,
}

fn make_payload(
    kind: &str,
    book_id: &str,
    device_id: &str,
    variant: usize,
    alt_key_order: bool,
) -> Value {
    match kind {
        "highlight" => {
            let color = (variant % 6) + 1;
            if alt_key_order {
                json!({
                    "extras": {"v": variant as f64},
                    "suffix": "",
                    "prefix": "",
                    "quote": format!("quote-{variant}"),
                    "color": color as f64
                })
            } else {
                json!({
                    "color": color,
                    "quote": format!("quote-{variant}"),
                    "prefix": "",
                    "suffix": "",
                    "extras": {"v": variant}
                })
            }
        }
        "note" => {
            if alt_key_order {
                json!({
                    "extras": {"n": variant as f64},
                    "body": format!("note-body-{variant}"),
                    "scope": "passage",
                    "ai": false,
                    "suffix": "",
                    "prefix": "",
                    "quote": "q"
                })
            } else {
                json!({
                    "quote": "q",
                    "prefix": "",
                    "suffix": "",
                    "ai": false,
                    "scope": "passage",
                    "body": format!("note-body-{variant}"),
                    "extras": {"n": variant}
                })
            }
        }
        "word" => json!({
            "word": format!("word-{variant}"),
            "definition": format!("def-{variant}"),
            "learned": variant.is_multiple_of(2),
            "extras": {}
        }),
        "lookup" => json!({
            "wordId": "w_alpha",
            "quote": format!("q-{variant}"),
            "prefix": "",
            "suffix": "",
            "sentence": "s",
            "date": "2026-11-28T00:00:00Z",
            "extras": {}
        }),
        "position" => json!({
            "bookId": book_id,
            "deviceId": device_id,
            "locator": {"format": "pdf", "page": (variant % 50) + 1},
            "extras": {}
        }),
        _ => unreachable!(),
    }
}

/// Reorders object keys without altering numeric types (preserves exact canonical request hash).
fn reorder_json_keys(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let keys: Vec<&String> = map.keys().collect();
            for k in keys.into_iter().rev() {
                out.insert(k.clone(), reorder_json_keys(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(reorder_json_keys).collect()),
        _ => v.clone(),
    }
}

/// Produces a JSON-equal payload with reversed key order and float-formatted integers (`1` -> `1.0`)
/// to test SYNC_SPEC §6 numeric & key-order invariance.
fn make_equivalent_payload(v: &Value) -> Value {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(serde_json::Number::from_f64(i as f64).unwrap())
            } else {
                v.clone()
            }
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let keys: Vec<&String> = map.keys().collect();
            for k in keys.into_iter().rev() {
                out.insert(k.clone(), make_equivalent_payload(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(make_equivalent_payload).collect()),
        _ => v.clone(),
    }
}

async fn run_seeded_trace(seed: u64, steps: usize, stats: &mut CoverageStats) {
    let mut rng = Rng::new(seed);
    let srv = TestServer::start().await;

    let devices = [
        ("dev_mac_1".to_owned(), srv.pair("dev_mac_1", "macos").await),
        (
            "dev_phone_1".to_owned(),
            srv.pair("dev_phone_1", "android").await,
        ),
        ("dev_mac_2".to_owned(), srv.pair("dev_mac_2", "macos").await),
    ];

    let mut model = RefModel::default();
    let mut next_id: u64 = 1;
    let mut next_id_str = |prefix: &str| {
        let id = format!("{prefix}_{seed:x}_{next_id}");
        next_id += 1;
        id
    };

    // Known book files we can register / upload blobs for
    let mut known_files: Vec<(Vec<u8>, String, &'static str, Option<String>)> = Vec::new();
    let mut sent_batches: Vec<(String, Value, Vec<u8>)> = Vec::new();
    let mut sent_changes: Vec<(String, Value)> = Vec::new();
    let mut sent_resolves: Vec<(String, String, Value)> = Vec::new();
    let mut resolved_conflict_ids: Vec<String> = Vec::new();

    // Seed initial books so step 1 already has live books to operate on.
    for init_i in 0..2 {
        let bytes = format!("book-bytes-{seed}-{init_i}").into_bytes();
        let sha = sha256_hex(&bytes);
        let fmt = if init_i == 0 { "pdf" } else { "epub" };
        let doc_id = Some(format!("doc_{init_i}"));
        let (dev_id, cred) = &devices[init_i % devices.len()];
        let req = json!({
            "sha256": sha,
            "size": bytes.len(),
            "format": fmt,
            "docId": doc_id,
            "title": format!("Book {init_i}"),
            "filename": format!("book_{init_i}.{fmt}"),
        });
        let (status, _, resp) = srv
            .send_json(Method::POST, cred, "/v1/books", Some(&req))
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "seed={seed} initial register failed"
        );
        model
            .apply_register(
                RegisterArgs {
                    device_id: dev_id,
                    sha: &sha,
                    size: bytes.len() as u64,
                    format: fmt,
                    doc_id: doc_id.as_deref(),
                    title: &format!("Book {init_i}"),
                    filename: &format!("book_{init_i}.{fmt}"),
                },
                &resp,
            )
            .unwrap_or_else(|e| panic!("seed={seed} initial register model error: {e}"));
        stats.register_created += 1;
        known_files.push((bytes, sha, fmt, doc_id));
    }

    let record_ids: [&str; 4] = ["rec_1", "rec_2", "w_caf\u{00e9}", "w_cafe\u{0301}"];
    let normal_kinds: [&str; 4] = ["highlight", "note", "word", "lookup"];

    for step in 0..steps {
        let dev_idx = rng.range(devices.len());
        let (dev_id, cred) = (&devices[dev_idx].0, &devices[dev_idx].1);
        let op = rng.range(100);

        if op < 10 {
            // Operation 1: Register book (new / duplicate / attach same docId / cross-format docId)
            let sub = rng.range(4);
            let (bytes, sha, fmt, doc_id) = match sub {
                0 => {
                    // Duplicate existing file
                    let existing = &known_files[rng.range(known_files.len())];
                    (
                        existing.0.clone(),
                        existing.1.clone(),
                        existing.2,
                        existing.3.clone(),
                    )
                }
                1 => {
                    // Attach new file version with same format + docId as existing book
                    let existing = &known_files[rng.range(known_files.len())];
                    let bytes = format!("book-rev-{seed}-{step}").into_bytes();
                    let sha = sha256_hex(&bytes);
                    (bytes, sha, existing.2, existing.3.clone())
                }
                2 => {
                    // Same docId but opposite format -> creates separate book
                    let existing = &known_files[rng.range(known_files.len())];
                    let fmt = if existing.2 == "pdf" { "epub" } else { "pdf" };
                    let bytes = format!("book-xfmt-{seed}-{step}").into_bytes();
                    let sha = sha256_hex(&bytes);
                    (bytes, sha, fmt, existing.3.clone())
                }
                _ => {
                    // Fresh book (no docId or empty docId)
                    let bytes = format!("book-new-{seed}-{step}").into_bytes();
                    let sha = sha256_hex(&bytes);
                    let fmt = if rng.chance(1, 2) { "pdf" } else { "epub" };
                    let doc_id = if rng.chance(1, 2) {
                        Some(String::new())
                    } else {
                        None
                    };
                    (bytes, sha, fmt, doc_id)
                }
            };
            let title = format!("Title {step}");
            let filename = format!("file_{step}.{fmt}");
            let req = json!({
                "sha256": sha,
                "size": bytes.len(),
                "format": fmt,
                "docId": doc_id,
                "title": title,
                "filename": filename,
            });
            let (status, _, resp) = srv
                .send_json(Method::POST, cred, "/v1/books", Some(&req))
                .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "seed={seed} step={step} register failed: {resp}"
            );
            match resp["status"].as_str() {
                Some("created") => stats.register_created += 1,
                Some("existing") => stats.register_existing += 1,
                Some("attached") => stats.register_attached += 1,
                other => panic!("seed={seed} step={step} unexpected register status: {other:?}"),
            }
            model
                .apply_register(
                    RegisterArgs {
                        device_id: dev_id,
                        sha: &sha,
                        size: bytes.len() as u64,
                        format: fmt,
                        doc_id: doc_id.as_deref(),
                        title: &title,
                        filename: &filename,
                    },
                    &resp,
                )
                .unwrap_or_else(|e| {
                    panic!("seed={seed} step={step} register refinement failed: {e}")
                });
            if !known_files.iter().any(|(_, s, _, _)| s == &sha) {
                known_files.push((bytes, sha, fmt, doc_id));
            }
        } else if op < 58 {
            // Operation 2: Push batch (1..=4 changes mixing creates, fast-forwards, noops, conflicts,
            // deletes, undeletes, positions, unknown book, reused changeId)
            let batch_id = next_id_str("bat");
            let count = 1 + rng.range(4);
            let mut batch_changes: Vec<Value> = Vec::new();
            let mut used_keys: BTreeSet<(String, String, String)> = BTreeSet::new();
            let mut used_cids: BTreeSet<String> = BTreeSet::new();
            let book_ids: Vec<String> = model.books.keys().cloned().collect();

            for _ in 0..count {
                let mode = rng.range(14);
                let chg_opt: Option<Value> = match mode {
                    0 => {
                        // Reuse a previously applied changeId inside a new batch (same OR different body)
                        if sent_changes.is_empty() {
                            None
                        } else {
                            let (old_cid, old_chg) = &sent_changes[rng.range(sent_changes.len())];
                            let k = (
                                old_chg["bookId"].as_str().unwrap().to_owned(),
                                old_chg["kind"].as_str().unwrap().to_owned(),
                                old_chg["id"].as_str().unwrap().to_owned(),
                            );
                            if used_cids.contains(old_cid) || used_keys.contains(&k) {
                                None
                            } else if rng.chance(1, 2) {
                                // Identical canonical body (reordered keys, exact same canonical hash)
                                stats.push_change_id_replayed += 1;
                                Some(reorder_json_keys(old_chg))
                            } else {
                                // Different body -> expect rejected change_id_reused
                                let mut mutated = old_chg.clone();
                                mutated["baseVersion"] =
                                    json!(mutated["baseVersion"].as_i64().unwrap() + 1);
                                Some(mutated)
                            }
                        }
                    }
                    1 => {
                        // Unknown book rejection
                        let fake_book = format!("sha256:{}", sha256_hex(format!("unreg-{step}")));
                        let k = (fake_book.clone(), "note".to_owned(), "n_ghost".to_owned());
                        if used_keys.contains(&k) {
                            None
                        } else {
                            let cid = next_id_str("chg");
                            Some(json!({
                                "changeId": cid,
                                "bookId": fake_book,
                                "kind": "note",
                                "id": "n_ghost",
                                "baseVersion": 0,
                                "deleted": false,
                                "clientTime": "2026-11-28T00:00:00Z",
                                "payload": make_payload("note", &fake_book, dev_id, 1, false),
                            }))
                        }
                    }
                    2..=3 => {
                        // Position update: valid owner, foreign owner, or delete rejection
                        let book = &book_ids[rng.range(book_ids.len())];
                        let sub = if stats.push_rejected_not_owner == 0 {
                            3
                        } else if stats.push_rejected_pos_delete == 0 {
                            4
                        } else {
                            rng.range(5)
                        };
                        let (pos_id, pay_dev, deleted, base) = match sub {
                            0..=2 => {
                                // Valid owner with arbitrary base (0, stale, exact, or future)
                                let base = rng.range(10) as i64;
                                (format!("p_{book}_{dev_id}"), dev_id.as_str(), false, base)
                            }
                            3 => {
                                // Foreign owner (either wrong id or wrong payload.deviceId)
                                let other = &devices[(dev_idx + 1) % devices.len()].0;
                                if rng.chance(1, 2) {
                                    (format!("p_{book}_{other}"), other.as_str(), false, 0)
                                } else {
                                    (format!("p_{book}_{dev_id}"), other.as_str(), false, 0)
                                }
                            }
                            _ => {
                                // Position delete rejection
                                (format!("p_{book}_{dev_id}"), dev_id.as_str(), true, 0)
                            }
                        };
                        let k = (book.clone(), "position".to_owned(), pos_id.clone());
                        if used_keys.contains(&k) {
                            None
                        } else {
                            let cid = next_id_str("chg");
                            let payload = if deleted {
                                Value::Null
                            } else {
                                // Sometimes send identical position to trigger position noop
                                let key = RecordKey {
                                    book_id: book.clone(),
                                    kind: "position".to_owned(),
                                    id: pos_id.clone(),
                                };
                                if let Some(existing) = model.records.get(&key)
                                    && rng.chance(1, 3)
                                    && let Some(ref p) = existing.payload
                                {
                                    make_equivalent_payload(p)
                                } else {
                                    make_payload(
                                        "position",
                                        book,
                                        pay_dev,
                                        rng.range(8),
                                        rng.chance(1, 2),
                                    )
                                }
                            };
                            Some(json!({
                                "changeId": cid,
                                "bookId": book,
                                "kind": "position",
                                "id": pos_id,
                                "baseVersion": base,
                                "deleted": deleted,
                                "payload": payload,
                            }))
                        }
                    }
                    _ => {
                        // Regular record: bias toward existing records (especially those with open
                        // conflicts) so conflict-append, conflict-noop, delete-vs-edit, and undelete
                        // transitions are densely exercised under every seed.
                        let existing_normal: Vec<RecordKey> = model
                            .records
                            .keys()
                            .filter(|k| k.kind != "book" && k.kind != "position")
                            .cloned()
                            .collect();
                        let open_conflict_keys: Vec<RecordKey> = model
                            .open_conflicts()
                            .into_iter()
                            .map(|c| c.key.clone())
                            .collect();
                        let (book, kind_owned, rec_id_owned) =
                            if !open_conflict_keys.is_empty() && rng.chance(2, 5) {
                                let k = &open_conflict_keys[rng.range(open_conflict_keys.len())];
                                (k.book_id.clone(), k.kind.clone(), k.id.clone())
                            } else if !existing_normal.is_empty() && rng.chance(3, 5) {
                                let k = &existing_normal[rng.range(existing_normal.len())];
                                (k.book_id.clone(), k.kind.clone(), k.id.clone())
                            } else {
                                let b = book_ids[rng.range(book_ids.len())].clone();
                                let k = normal_kinds[rng.range(normal_kinds.len())].to_owned();
                                let id = record_ids[rng.range(record_ids.len())].to_owned();
                                (b, k, id)
                            };
                        let kind = kind_owned.as_str();
                        let rec_id = rec_id_owned.as_str();
                        let k = (book.clone(), kind.to_owned(), rec_id.to_owned());
                        if used_keys.contains(&k) {
                            None
                        } else {
                            let rkey = RecordKey {
                                book_id: book.clone(),
                                kind: kind.to_owned(),
                                id: rec_id.to_owned(),
                            };
                            let cid = next_id_str("chg");
                            let existing = model.records.get(&rkey).cloned();
                            let (base, deleted, payload) = match existing {
                                None => {
                                    if rng.chance(1, 6) {
                                        // Absent record with base > 0 -> bad_base
                                        (
                                            1 + rng.range(3) as i64,
                                            false,
                                            make_payload(kind, &book, dev_id, rng.range(10), false),
                                        )
                                    } else {
                                        // Clean create (base=0)
                                        (
                                            0,
                                            false,
                                            make_payload(
                                                kind,
                                                &book,
                                                dev_id,
                                                rng.range(6),
                                                rng.chance(1, 2),
                                            ),
                                        )
                                    }
                                }
                                Some(head) => {
                                    let action = rng.range(10);
                                    match action {
                                        0..=1 => {
                                            // Fast-forward live edit (or undelete if head is tombstone!)
                                            (
                                                head.version,
                                                false,
                                                make_payload(
                                                    kind,
                                                    &book,
                                                    dev_id,
                                                    rng.range(10),
                                                    rng.chance(1, 2),
                                                ),
                                            )
                                        }
                                        2 => {
                                            // Delete (fast-forward if base==head, delete-vs-delete noop if tombstone,
                                            // or edit-vs-delete conflict if base < head!)
                                            let base = if rng.chance(1, 2) {
                                                head.version
                                            } else {
                                                rng.range(head.version as usize) as i64
                                            };
                                            (base, true, Value::Null)
                                        }
                                        3..=4 => {
                                            // Identical-content noop with arbitrary base (including future base!)
                                            let base =
                                                rng.range((head.version as usize) + 4) as i64;
                                            let pay = if head.deleted {
                                                Value::Null
                                            } else {
                                                make_equivalent_payload(
                                                    head.payload.as_ref().unwrap(),
                                                )
                                            };
                                            (base, head.deleted, pay)
                                        }
                                        5..=8 => {
                                            // Stale-base conflict (open, append, or match open side noop!)
                                            let base = rng.range(head.version as usize) as i64;
                                            if let Some(ref open_cid) = head.open_conflict_id
                                                && rng.chance(1, 3)
                                            {
                                                let cf = &model.conflicts[open_cid];
                                                let side = &cf.sides[rng.range(cf.sides.len())];
                                                let pay = side
                                                    .payload
                                                    .as_ref()
                                                    .map_or(Value::Null, make_equivalent_payload);
                                                (base, side.deleted, pay)
                                            } else {
                                                let del = rng.chance(1, 4);
                                                let pay = if del {
                                                    Value::Null
                                                } else {
                                                    make_payload(
                                                        kind,
                                                        &book,
                                                        dev_id,
                                                        100 + step * 5 + rng.range(20),
                                                        false,
                                                    )
                                                };
                                                (base, del, pay)
                                            }
                                        }
                                        _ => {
                                            // Future-base rejection with distinct payload
                                            (
                                                head.version + 1 + rng.range(3) as i64,
                                                false,
                                                make_payload(
                                                    kind,
                                                    &book,
                                                    dev_id,
                                                    200 + rng.range(20),
                                                    false,
                                                ),
                                            )
                                        }
                                    }
                                }
                            };
                            Some(json!({
                                "changeId": cid,
                                "bookId": book,
                                "kind": kind,
                                "id": rec_id,
                                "baseVersion": base,
                                "deleted": deleted,
                                "clientTime": "2026-11-28T00:00:00Z",
                                "payload": payload,
                            }))
                        }
                    }
                };

                if let Some(chg) = chg_opt {
                    used_keys.insert((
                        chg["bookId"].as_str().unwrap().to_owned(),
                        chg["kind"].as_str().unwrap().to_owned(),
                        chg["id"].as_str().unwrap().to_owned(),
                    ));
                    used_cids.insert(chg["changeId"].as_str().unwrap().to_owned());
                    batch_changes.push(chg);
                }
            }

            if batch_changes.is_empty() {
                continue;
            }
            let batch_body = json!({ "batchId": batch_id, "changes": batch_changes });
            let (status, resp_bytes, resp_json) = srv
                .send_json(Method::POST, cred, "/v1/changes", Some(&batch_body))
                .await;

            // Record coverage stats before mutating model
            if status == StatusCode::OK
                && let Some(results) = resp_json["results"].as_array()
            {
                for r in results {
                    match r["status"].as_str() {
                        Some("applied") => stats.push_applied += 1,
                        Some("noop") => {
                            if r.get("conflictId").is_some() {
                                stats.push_noop_conflict += 1;
                            } else {
                                stats.push_noop_head += 1;
                            }
                        }
                        Some("conflict") => {
                            let cid = r["conflictId"].as_str().unwrap();
                            if model.conflicts.contains_key(cid) {
                                stats.push_conflict_append += 1;
                            } else {
                                stats.push_conflict_open += 1;
                            }
                        }
                        Some("rejected") => match r["error"]["code"].as_str() {
                            Some("bad_base") => stats.push_rejected_bad_base += 1,
                            Some("unknown_book") => stats.push_rejected_unknown_book += 1,
                            Some("not_owner") => stats.push_rejected_not_owner += 1,
                            Some("bad_request") => stats.push_rejected_pos_delete += 1,
                            Some("change_id_reused") => stats.push_rejected_change_id_reused += 1,
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }

            model
                .apply_push_batch(dev_id, &batch_body, status, &resp_bytes, &resp_json)
                .unwrap_or_else(|e| {
                    panic!(
                        "seed={seed} step={step} push refinement failed: {e}\nbatch={batch_body}\nresp={resp_json}"
                    )
                });

            if status == StatusCode::OK {
                sent_batches.push((batch_id, batch_body.clone(), resp_bytes));
                for chg in batch_body["changes"].as_array().unwrap() {
                    let cid = chg["changeId"].as_str().unwrap().to_owned();
                    if !sent_changes.iter().any(|(c, _)| c == &cid) {
                        sent_changes.push((cid, chg.clone()));
                    }
                }
            }
        } else if op < 68 && !sent_batches.is_empty() {
            // Operation 3: Batch replay (identical body with reordered formatting -> byte-identical 200;
            // OR same batchId with mutated body -> 422 change_id_reused)
            let (bid, orig_body, _) = sent_batches[rng.range(sent_batches.len())].clone();
            if rng.chance(1, 2) {
                // Identical canonical body serialized with pretty formatting (extra whitespace & reordered keys)
                let pretty = serde_json::to_string_pretty(&reorder_json_keys(&orig_body)).unwrap();
                let (status, resp_bytes, resp_json) = srv
                    .send_raw_json(Method::POST, cred, "/v1/changes", pretty)
                    .await;
                model
                    .apply_push_batch(dev_id, &orig_body, status, &resp_bytes, &resp_json)
                    .unwrap_or_else(|e| panic!("seed={seed} step={step} batch replay failed: {e}"));
                stats.batch_replayed_ok += 1;
            } else {
                // Mutated body with same batchId -> 422 change_id_reused
                let book = model.books.keys().next().unwrap().clone();
                let mutated = json!({
                    "batchId": bid,
                    "changes": [{
                        "changeId": next_id_str("mut"),
                        "bookId": book,
                        "kind": "note",
                        "id": "n_mutated",
                        "baseVersion": 0,
                        "deleted": false,
                        "payload": make_payload("note", &book, dev_id, 999, false),
                    }]
                });
                let (status, resp_bytes, resp_json) = srv
                    .send_json(Method::POST, cred, "/v1/changes", Some(&mutated))
                    .await;
                model
                    .apply_push_batch(dev_id, &mutated, status, &resp_bytes, &resp_json)
                    .unwrap_or_else(|e| {
                        panic!("seed={seed} step={step} batch 422 check failed: {e}")
                    });
                stats.batch_reused_422 += 1;
            }
        } else if op < 84 {
            // Operation 4: Conflict resolve (valid, stale_base 409, already_resolved 409,
            // idempotent replay, reused changeId with different body -> 422)
            let open_ids: Vec<String> = model
                .open_conflicts()
                .into_iter()
                .map(|c| c.conflict_id.clone())
                .collect();
            let sub = if !open_ids.is_empty() && stats.resolve_stale_409 == 0 {
                2
            } else if !open_ids.is_empty() && stats.resolve_applied == 0 {
                3
            } else if !sent_resolves.is_empty() && stats.resolve_replayed_ok == 0 {
                0
            } else if !resolved_conflict_ids.is_empty() && stats.resolve_already_409 == 0 {
                1
            } else if !sent_resolves.is_empty() && stats.resolve_reused_422 == 0 {
                4
            } else if open_ids.is_empty() && !sent_resolves.is_empty() {
                match rng.range(3) {
                    0 => 0,
                    1 => 1,
                    _ => 4,
                }
            } else {
                rng.range(5)
            };
            if sub == 0 && !sent_resolves.is_empty() {
                // Idempotent replay of a previous resolve
                let (cid, _, body) = sent_resolves[rng.range(sent_resolves.len())].clone();
                let (status, resp_bytes, resp_json) = srv
                    .send_json(
                        Method::POST,
                        cred,
                        &format!("/v1/conflicts/{cid}/resolve"),
                        Some(&body),
                    )
                    .await;
                model
                    .apply_resolve(dev_id, &cid, &body, status, &resp_bytes, &resp_json)
                    .unwrap_or_else(|e| {
                        panic!("seed={seed} step={step} resolve replay failed: {e}")
                    });
                stats.resolve_replayed_ok += 1;
            } else if sub == 4 && !sent_resolves.is_empty() {
                // Reused changeId with a different resolve body -> 422 change_id_reused
                let (cid, _, mut body) = sent_resolves[rng.range(sent_resolves.len())].clone();
                body["baseVersion"] = json!(body["baseVersion"].as_i64().unwrap() + 1);
                let (status, resp_bytes, resp_json) = srv
                    .send_json(
                        Method::POST,
                        cred,
                        &format!("/v1/conflicts/{cid}/resolve"),
                        Some(&body),
                    )
                    .await;
                model
                    .apply_resolve(dev_id, &cid, &body, status, &resp_bytes, &resp_json)
                    .unwrap_or_else(|e| panic!("seed={seed} step={step} resolve 422 failed: {e}"));
                stats.resolve_reused_422 += 1;
            } else if sub == 1 && !resolved_conflict_ids.is_empty() {
                // Already resolved -> 409 already_resolved
                let cid = resolved_conflict_ids[rng.range(resolved_conflict_ids.len())].clone();
                let body = json!({
                    "changeId": next_id_str("res_again"),
                    "baseVersion": 1,
                    "deleted": false,
                    "payload": make_payload("note", "", dev_id, 55, false),
                });
                let (status, resp_bytes, resp_json) = srv
                    .send_json(
                        Method::POST,
                        cred,
                        &format!("/v1/conflicts/{cid}/resolve"),
                        Some(&body),
                    )
                    .await;
                model
                    .apply_resolve(dev_id, &cid, &body, status, &resp_bytes, &resp_json)
                    .unwrap_or_else(|e| {
                        panic!("seed={seed} step={step} already_resolved failed: {e}")
                    });
                stats.resolve_already_409 += 1;
            } else if !open_ids.is_empty() {
                let cid = open_ids[rng.range(open_ids.len())].clone();
                let cf = model.conflicts[&cid].clone();
                let head = model.records[&cf.key].clone();
                let stale = sub == 2 || (sub != 3 && rng.chance(1, 2));
                let base_ver = if stale {
                    if head.version > 1 {
                        head.version - 1
                    } else {
                        head.version + 1
                    }
                } else {
                    head.version
                };
                let deleted = rng.chance(1, 4);
                let payload = if deleted {
                    Value::Null
                } else {
                    make_payload(&cf.key.kind, &cf.key.book_id, dev_id, 300 + step, false)
                };
                let rcid = next_id_str("res");
                let body = json!({
                    "changeId": rcid,
                    "baseVersion": base_ver,
                    "deleted": deleted,
                    "payload": payload,
                });
                let (status, resp_bytes, resp_json) = srv
                    .send_json(
                        Method::POST,
                        cred,
                        &format!("/v1/conflicts/{cid}/resolve"),
                        Some(&body),
                    )
                    .await;
                model
                    .apply_resolve(dev_id, &cid, &body, status, &resp_bytes, &resp_json)
                    .unwrap_or_else(|e| panic!("seed={seed} step={step} resolve failed: {e}"));
                if stale {
                    stats.resolve_stale_409 += 1;
                } else {
                    stats.resolve_applied += 1;
                    sent_resolves.push((cid.clone(), rcid, body));
                    resolved_conflict_ids.push(cid);
                }
            }
        } else if op < 94 {
            // Operation 5: Blob PUT under normal OR simulated disk_full
            let file_idx = rng.range(known_files.len());
            let (bytes, sha) = (
                known_files[file_idx].0.clone(),
                known_files[file_idx].1.clone(),
            );
            let already_stored = model.blobs.contains_key(&sha);
            let simulate_full = rng.chance(1, 2);
            if simulate_full {
                srv.storage.set_free_bytes(FULL_FREE_BYTES);
            } else {
                srv.storage.set_free_bytes(HEALTHY_FREE_BYTES);
            }

            let resp = srv
                .http
                .put(srv.url(&format!("/v1/blobs/{sha}")))
                .bearer_auth(cred)
                .body(bytes.clone())
                .send()
                .await
                .expect("put blob");
            let status = resp.status();
            let resp_body: Value = resp.json().await.unwrap_or(Value::Null);

            if already_stored {
                // Duplicate PUT always succeeds with 200 without reading body (even when disk_full!)
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "seed={seed} step={step} duplicate blob put failed: {resp_body}"
                );
                stats.blob_deduped += 1;
            } else if simulate_full {
                // Below free_floor_bytes -> 507 disk_full; blob NOT stored; record push/pull MUST still work!
                assert_eq!(
                    status,
                    StatusCode::INSUFFICIENT_STORAGE,
                    "seed={seed} step={step} expected 507 disk_full, got {status} {resp_body}"
                );
                assert_eq!(resp_body["error"]["code"].as_str(), Some("disk_full"));
                stats.blob_disk_full_507 += 1;

                // Immediately push a record while disk_full is STILL active!
                let book = model.books.keys().next().unwrap().clone();
                let cid = next_id_str("df_chg");
                let bid = next_id_str("df_bat");
                let rec_id = format!("n_df_{step}");
                let chg = json!({
                    "changeId": cid,
                    "bookId": book,
                    "kind": "note",
                    "id": rec_id,
                    "baseVersion": 0,
                    "deleted": false,
                    "clientTime": "2026-11-28T00:00:00Z",
                    "payload": make_payload("note", &book, dev_id, step, false),
                });
                let batch_body = json!({ "batchId": bid, "changes": [chg] });
                let (p_status, p_bytes, p_json) = srv
                    .send_json(Method::POST, cred, "/v1/changes", Some(&batch_body))
                    .await;
                assert_eq!(
                    p_status,
                    StatusCode::OK,
                    "seed={seed} step={step} disk_full blocked record push: {p_json}"
                );
                model
                    .apply_push_batch(dev_id, &batch_body, p_status, &p_bytes, &p_json)
                    .unwrap_or_else(|e| {
                        panic!("seed={seed} step={step} disk_full push failed: {e}")
                    });
                // Also verify pull works while disk_full is active!
                let (pull_status, _, _) = srv
                    .send_json(Method::GET, cred, "/v1/changes?since=0&limit=5", None)
                    .await;
                assert_eq!(
                    pull_status,
                    StatusCode::OK,
                    "seed={seed} step={step} disk_full blocked pull"
                );
                // Restore healthy free space after verifying disk_full behavior.
                srv.storage.set_free_bytes(HEALTHY_FREE_BYTES);
            } else {
                assert_eq!(
                    status,
                    StatusCode::CREATED,
                    "seed={seed} step={step} expected 201 blob created, got {status} {resp_body}"
                );
                model.blobs.insert(sha.clone(), bytes);
                stats.blob_created += 1;
            }
        } else {
            // Operation 6: Mid-cursor paginated pull check
            let since = if model.seq > 0 {
                rng.range((model.seq as usize) + 1) as i64
            } else {
                0
            };
            let limit = 1 + rng.range(4);
            let mut cur = since;
            let mut pulled: Vec<Value> = Vec::new();
            loop {
                let (status, _, page) = srv
                    .send_json(
                        Method::GET,
                        cred,
                        &format!("/v1/changes?since={cur}&limit={limit}"),
                        None,
                    )
                    .await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "seed={seed} step={step} mid-pull failed"
                );
                let items = page["changes"].as_array().unwrap();
                for item in items {
                    assert!(item["seq"].as_i64().unwrap() > cur);
                    pulled.push(item.clone());
                }
                cur = page["cursor"].as_i64().unwrap();
                if !page["hasMore"].as_bool().unwrap() {
                    break;
                }
            }
            let expected_count = model.records.values().filter(|r| r.seq > since).count();
            assert_eq!(
                pulled.len(),
                expected_count,
                "seed={seed} step={step} mid-cursor pull (since={since}) count mismatch"
            );
            assert!(
                pulled
                    .windows(2)
                    .all(|w| w[0]["seq"].as_i64().unwrap() < w[1]["seq"].as_i64().unwrap()),
                "seed={seed} step={step} mid-cursor pull seq not strictly increasing"
            );
            stats.pull_mid_cursor += 1;
        }

        // Verify FULL state refinement + all SYNC_SPEC §6 & §12.3 invariants after EVERY step!
        let page_limit = 1 + rng.range(5);
        let snap = capture_snapshot(&srv, cred, &model, page_limit).await;
        verify_invariants(&model, &snap)
            .unwrap_or_else(|err| panic!("INVARIANT VIOLATION at seed={seed} step={step}: {err}"));
    }

    srv.stop().await;
}

#[tokio::test]
async fn reader_sync_stateful_refinement_and_invariants() {
    let base_seed = env_seed();
    let total_steps = env_total_steps();
    let runs = 4usize.min(total_steps.max(1));
    let steps_per_run = total_steps.div_ceil(runs);

    let mut stats = CoverageStats::default();
    for run in 0..runs {
        let seed = base_seed.wrapping_add((run as u64).wrapping_mul(0x1000_0001));
        run_seeded_trace(seed, steps_per_run, &mut stats).await;
    }

    // Ensure every critical protocol path was actually exercised when running >= 80 steps.
    if total_steps >= 150 {
        assert!(stats.register_created > 0, "no register_created: {stats:?}");
        assert!(
            stats.register_existing > 0,
            "no register_existing: {stats:?}"
        );
        assert!(
            stats.register_attached > 0,
            "no register_attached: {stats:?}"
        );
        assert!(stats.push_applied > 0, "no push_applied: {stats:?}");
        assert!(stats.push_noop_head > 0, "no push_noop_head: {stats:?}");
        assert!(
            stats.push_conflict_open > 0,
            "no push_conflict_open: {stats:?}"
        );
        assert!(
            stats.push_conflict_append > 0,
            "no push_conflict_append: {stats:?}"
        );
        assert!(stats.push_rejected_bad_base > 0, "no bad_base: {stats:?}");
        assert!(
            stats.push_rejected_unknown_book > 0,
            "no unknown_book: {stats:?}"
        );
        assert!(stats.push_rejected_not_owner > 0, "no not_owner: {stats:?}");
        assert!(
            stats.push_rejected_pos_delete > 0,
            "no pos_delete: {stats:?}"
        );
        assert!(
            stats.push_rejected_change_id_reused > 0,
            "no change_id_reused: {stats:?}"
        );
        assert!(
            stats.push_change_id_replayed > 0,
            "no change_id_replayed: {stats:?}"
        );
        assert!(
            stats.batch_replayed_ok > 0,
            "no batch_replayed_ok: {stats:?}"
        );
        assert!(stats.batch_reused_422 > 0, "no batch_reused_422: {stats:?}");
        assert!(stats.resolve_applied > 0, "no resolve_applied: {stats:?}");
        assert!(
            stats.resolve_stale_409 > 0,
            "no resolve_stale_409: {stats:?}"
        );
        assert!(
            stats.resolve_already_409 > 0,
            "no resolve_already_409: {stats:?}"
        );
        assert!(
            stats.resolve_replayed_ok > 0,
            "no resolve_replayed_ok: {stats:?}"
        );
        assert!(
            stats.resolve_reused_422 > 0,
            "no resolve_reused_422: {stats:?}"
        );
        assert!(stats.blob_created > 0, "no blob_created: {stats:?}");
        assert!(
            stats.blob_disk_full_507 > 0,
            "no blob_disk_full_507: {stats:?}"
        );
        assert!(stats.pull_mid_cursor > 0, "no pull_mid_cursor: {stats:?}");
    }
}

/// Canary self-test confirming the invariant oracle fails loudly when fed broken server states.
#[test]
fn canary_invariant_oracle_rejects_broken_states() {
    let book_id = format!("sha256:{}", "aa".repeat(32));
    let key = RecordKey {
        book_id: book_id.clone(),
        kind: "note".to_owned(),
        id: "n_1".to_owned(),
    };
    let pay_a = json!({"body": "a", "extras": {}});
    let pay_b = json!({"body": "b", "extras": {}});

    // Build a valid baseline model + matching snapshot first.
    let mut model = RefModel {
        seq: 2,
        ..RefModel::default()
    };
    let mut versions = BTreeMap::new();
    versions.insert(
        1,
        ModelVersion {
            deleted: false,
            payload: Some(pay_a.clone()),
            seq: 1,
        },
    );
    model.records.insert(
        key.clone(),
        ModelRecord {
            version: 1,
            deleted: false,
            payload: Some(pay_a.clone()),
            device_id: "dev_a".to_owned(),
            change_id: "c1".to_owned(),
            client_time: None,
            seq: 2,
            open_conflict_id: Some("cf_1".to_owned()),
            versions,
        },
    );
    model.conflicts.insert(
        "cf_1".to_owned(),
        ModelConflict {
            conflict_id: "cf_1".to_owned(),
            key: key.clone(),
            base_version: 0,
            opened_seq: 2,
            resolved_seq: None,
            sides: vec![
                ModelSide {
                    change_id: "c1".to_owned(),
                    device_id: "dev_a".to_owned(),
                    version: Some(1),
                    deleted: false,
                    payload: Some(pay_a.clone()),
                    client_time: None,
                },
                ModelSide {
                    change_id: "c2".to_owned(),
                    device_id: "dev_b".to_owned(),
                    version: None,
                    deleted: false,
                    payload: Some(pay_b.clone()),
                    client_time: None,
                },
            ],
        },
    );

    let good_conflict = json!({
        "conflictId": "cf_1",
        "bookId": book_id,
        "kind": "note",
        "id": "n_1",
        "baseVersion": 0,
        "base": null,
        "openedSeq": 2,
        "sides": [
            {"changeId": "c1", "deviceId": "dev_a", "version": 1, "deleted": false, "payload": pay_a, "clientTime": null},
            {"changeId": "c2", "deviceId": "dev_b", "version": null, "deleted": false, "payload": pay_b, "clientTime": null},
        ]
    });
    let mut db_versions = BTreeMap::new();
    db_versions.insert(
        key.clone(),
        vec![DbVersionRow {
            version: 1,
            deleted: false,
            payload: Some(pay_a.clone()),
            seq: 1,
        }],
    );
    let good_snap = ServerSnapshot {
        health_seq: 2,
        feed_records: vec![json!({
            "bookId": book_id,
            "kind": "note",
            "id": "n_1",
            "version": 1,
            "deleted": false,
            "deviceId": "dev_a",
            "changeId": "c1",
            "clientTime": null,
            "seq": 2,
            "payload": pay_a,
            "conflict": good_conflict,
        })],
        open_conflicts: vec![good_conflict.clone()],
        books: vec![],
        db_versions,
        verified_blobs: BTreeMap::new(),
    };

    assert_eq!(
        verify_invariants(&model, &good_snap),
        Ok(()),
        "good baseline must pass"
    );

    // Canary 1: conflict with two identical (`same`) sides MUST fail!
    let mut same_sides_model = model.clone();
    same_sides_model.conflicts.get_mut("cf_1").unwrap().sides[1].payload = Some(pay_a.clone());
    let mut same_sides_cf = good_conflict.clone();
    same_sides_cf["sides"][1]["payload"] = pay_a.clone();
    let mut bad1 = good_snap.clone();
    bad1.feed_records[0]["conflict"] = same_sides_cf.clone();
    bad1.open_conflicts = vec![same_sides_cf];
    assert!(
        verify_invariants(&same_sides_model, &bad1).is_err(),
        "canary 1: oracle must reject conflict whose sides are same"
    );

    // Canary 2: duplicate / non-increasing seq MUST fail!
    let mut bad2 = good_snap.clone();
    let dup = bad2.feed_records[0].clone();
    bad2.feed_records.push(dup);
    assert!(
        verify_invariants(&model, &bad2).is_err(),
        "canary 2: oracle must reject duplicate seq"
    );

    // Canary 3: missing record_versions history gap MUST fail!
    let mut bad3 = good_snap.clone();
    bad3.db_versions.get_mut(&key).unwrap().clear();
    assert!(
        verify_invariants(&model, &bad3).is_err(),
        "canary 3: oracle must reject missing record_versions entry"
    );

    // Canary 4: head payload divergence from model MUST fail!
    let mut bad4 = good_snap.clone();
    bad4.feed_records[0]["payload"] = json!({"body": "corrupted", "extras": {}});
    assert!(
        verify_invariants(&model, &bad4).is_err(),
        "canary 4: oracle must reject head payload mismatch"
    );

    // Canary 5: position record with an open conflict MUST fail!
    let pos_key = RecordKey {
        book_id: book_id.clone(),
        kind: "position".to_owned(),
        id: format!("p_{book_id}_dev_a"),
    };
    let pos_pay = json!({"bookId": book_id, "deviceId": "dev_a", "locator": {"format": "pdf", "page": 1}, "extras": {}});
    let mut pos_model = RefModel {
        seq: 1,
        ..RefModel::default()
    };
    let mut pvers = BTreeMap::new();
    pvers.insert(
        1,
        ModelVersion {
            deleted: false,
            payload: Some(pos_pay.clone()),
            seq: 1,
        },
    );
    pos_model.records.insert(
        pos_key.clone(),
        ModelRecord {
            version: 1,
            deleted: false,
            payload: Some(pos_pay.clone()),
            device_id: "dev_a".to_owned(),
            change_id: "p1".to_owned(),
            client_time: None,
            seq: 1,
            open_conflict_id: None,
            versions: pvers,
        },
    );
    let mut p_db = BTreeMap::new();
    p_db.insert(
        pos_key,
        vec![DbVersionRow {
            version: 1,
            deleted: false,
            payload: Some(pos_pay.clone()),
            seq: 1,
        }],
    );
    let bad5 = ServerSnapshot {
        health_seq: 1,
        feed_records: vec![json!({
            "bookId": book_id,
            "kind": "position",
            "id": format!("p_{book_id}_dev_a"),
            "version": 1,
            "deleted": false,
            "deviceId": "dev_a",
            "changeId": "p1",
            "clientTime": null,
            "seq": 1,
            "payload": pos_pay,
            "conflict": good_conflict,
        })],
        open_conflicts: vec![],
        books: vec![],
        db_versions: p_db,
        verified_blobs: BTreeMap::new(),
    };
    assert!(
        verify_invariants(&pos_model, &bad5).is_err(),
        "canary 5: oracle must reject a position with an open conflict"
    );
}

#[tokio::test]
async fn regression_resolve_rejects_change_id_reused_across_different_conflict() {
    let srv = TestServer::start().await;
    let a = srv.pair("dev_a", "macos").await;
    let b = srv.pair("dev_b", "android").await;
    let file = b"conflict-probe-book";
    let sha = sha256_hex(file);
    let book_id = format!("sha256:{sha}");
    let (s, _, _) = srv
        .send_json(
            Method::POST,
            &a,
            "/v1/books",
            Some(&json!({
                "sha256": sha, "size": file.len(), "format": "pdf",
                "title": "Probe", "filename": "probe.pdf"
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    // Create two distinct open conflicts: cf1 on n_1 and cf2 on n_2 (both at head version 1).
    let (_, _, r1) = srv
        .send_json(
            Method::POST,
            &a,
            "/v1/changes",
            Some(&json!({
                "batchId": "b_init",
                "changes": [
                    {"changeId": "c1", "bookId": book_id, "kind": "note", "id": "n_1", "baseVersion": 0, "deleted": false, "payload": {"body": "a1", "extras": {}}},
                    {"changeId": "c2", "bookId": book_id, "kind": "note", "id": "n_2", "baseVersion": 0, "deleted": false, "payload": {"body": "a2", "extras": {}}}
                ]
            })),
        )
        .await;
    assert_eq!(r1["results"][0]["status"], "applied");
    assert_eq!(r1["results"][1]["status"], "applied");

    let (_, _, r2) = srv
        .send_json(
            Method::POST,
            &b,
            "/v1/changes",
            Some(&json!({
                "batchId": "b_conf",
                "changes": [
                    {"changeId": "c3", "bookId": book_id, "kind": "note", "id": "n_1", "baseVersion": 0, "deleted": false, "payload": {"body": "b1", "extras": {}}},
                    {"changeId": "c4", "bookId": book_id, "kind": "note", "id": "n_2", "baseVersion": 0, "deleted": false, "payload": {"body": "b2", "extras": {}}}
                ]
            })),
        )
        .await;
    let cf1 = r2["results"][0]["conflictId"].as_str().unwrap().to_owned();
    let cf2 = r2["results"][1]["conflictId"].as_str().unwrap().to_owned();
    assert_ne!(cf1, cf2);

    // Resolve cf1 with changeId = "res_shared".
    let resolve_body = json!({
        "changeId": "res_shared",
        "baseVersion": 1,
        "deleted": false,
        "payload": {"body": "merged", "extras": {}}
    });
    let (s1, _, body1) = srv
        .send_json(
            Method::POST,
            &a,
            &format!("/v1/conflicts/{cf1}/resolve"),
            Some(&resolve_body),
        )
        .await;
    assert_eq!(s1, StatusCode::OK, "resolve cf1: {body1}");

    // Now send the EXACT SAME body (same changeId!) to cf2 (a DIFFERENT open conflict!).
    let (s2, _, body2) = srv
        .send_json(
            Method::POST,
            &a,
            &format!("/v1/conflicts/{cf2}/resolve"),
            Some(&resolve_body),
        )
        .await;
    let (_, _, open) = srv.send_json(Method::GET, &a, "/v1/conflicts", None).await;
    srv.stop().await;

    assert_eq!(
        s2,
        StatusCode::UNPROCESSABLE_ENTITY,
        "CROSS-CONFLICT REUSE BUG CONFIRMED: POST /v1/conflicts/{cf2}/resolve with cf1's changeId returned {s2} {body2} while open conflicts remain: {open}"
    );
}

#[tokio::test]
async fn regression_push_rejects_live_record_with_null_payload() {
    let srv = TestServer::start().await;
    let a = srv.pair("dev_a", "macos").await;
    let file = b"null-payload-probe-book";
    let sha = sha256_hex(file);
    let book_id = format!("sha256:{sha}");
    let (s, _, _) = srv
        .send_json(
            Method::POST,
            &a,
            "/v1/books",
            Some(&json!({
                "sha256": sha, "size": file.len(), "format": "pdf",
                "title": "Probe2", "filename": "probe2.pdf"
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    let (push_status, _, push_body) = srv
        .send_json(
            Method::POST,
            &a,
            "/v1/changes",
            Some(&json!({
                "batchId": "b_null_live",
                "changes": [{
                    "changeId": "c_null_live",
                    "bookId": book_id,
                    "kind": "note",
                    "id": "n_null_live",
                    "baseVersion": 0,
                    "deleted": false,
                    "payload": null
                }]
            })),
        )
        .await;
    let (_, _, feed) = srv
        .send_json(Method::GET, &a, "/v1/changes?since=0&limit=50", None)
        .await;
    srv.stop().await;

    assert_eq!(
        push_status,
        StatusCode::BAD_REQUEST,
        "LIVE NULL PAYLOAD BUG CONFIRMED: POST /v1/changes accepted deleted=false, payload=null with {push_status} {push_body}; feed={feed}"
    );
}

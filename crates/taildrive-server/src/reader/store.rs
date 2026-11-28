//! `reader.db`: the Reader sync record store (SYNC_SPEC §5, §6, §8). Separate from TailDrive's
//! `taildrive.db`; TailDrive's `files` table and its conflict-copy logic never touch records.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use taildrive_core::hash::sha256_hex;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::task::spawn_blocking;

use super::{
    error::{SyncError, SyncResult},
    wire::{self, Decision, Head, Incoming, Side},
};

pub(crate) const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_CHANGES: usize = 500;
const PAIRING_TTL_SECONDS: i64 = 10 * 60;
pub(crate) const PROTOCOL: i64 = 1;
pub(crate) const SERVER_VERSION: &str = "1.0.0";

/// Versioned, idempotent migrations. `PRAGMA user_version` is the applied version; each step runs
/// in its own transaction, so a crash mid-migration leaves the previous version intact.
const MIGRATIONS: &[&str] = &[
    // v1: records, conflicts, idempotency, devices, pairing, book indexes (§8.1).
    "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE devices (
        id TEXT PRIMARY KEY, name TEXT NOT NULL, platform TEXT NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('mac', 'phone')), app_version TEXT,
        credential_hash TEXT UNIQUE, created_at TEXT NOT NULL, last_seen_at TEXT, revoked_at TEXT
    );
    CREATE TABLE pairing_tokens (
        token_hash TEXT PRIMARY KEY, kind TEXT NOT NULL, issued_by TEXT, expires_at INTEGER NOT NULL,
        redeemed_device_id TEXT, redeemed_at TEXT
    );
    CREATE TABLE records (
        book_id TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL, version INTEGER NOT NULL,
        deleted INTEGER NOT NULL, payload TEXT, device_id TEXT NOT NULL, change_id TEXT NOT NULL,
        client_time TEXT, seq INTEGER NOT NULL UNIQUE, conflict_id TEXT,
        PRIMARY KEY (book_id, kind, id)
    );
    CREATE TABLE record_versions (
        book_id TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL, version INTEGER NOT NULL,
        deleted INTEGER NOT NULL, payload TEXT, device_id TEXT NOT NULL, change_id TEXT NOT NULL,
        seq INTEGER NOT NULL, PRIMARY KEY (book_id, kind, id, version)
    );
    CREATE TABLE book_files (sha256 TEXT PRIMARY KEY, book_id TEXT NOT NULL, size INTEGER NOT NULL, added_at TEXT NOT NULL);
    CREATE INDEX book_files_book ON book_files (book_id);
    CREATE TABLE book_doc_ids (format TEXT NOT NULL, doc_id TEXT NOT NULL, book_id TEXT NOT NULL, PRIMARY KEY (format, doc_id));
    CREATE TABLE conflicts (
        id TEXT PRIMARY KEY, book_id TEXT NOT NULL, kind TEXT NOT NULL, record_id TEXT NOT NULL,
        base_version INTEGER NOT NULL, opened_seq INTEGER NOT NULL, resolved_seq INTEGER, resolved_change_id TEXT
    );
    CREATE INDEX conflicts_open ON conflicts (resolved_seq, book_id, opened_seq);
    CREATE TABLE conflict_sides (
        conflict_id TEXT NOT NULL, side INTEGER NOT NULL, change_id TEXT NOT NULL, device_id TEXT NOT NULL,
        version INTEGER, deleted INTEGER NOT NULL, payload TEXT, client_time TEXT,
        PRIMARY KEY (conflict_id, side)
    );
    CREATE TABLE applied_changes (change_id TEXT PRIMARY KEY, device_id TEXT NOT NULL, request_hash TEXT NOT NULL, result TEXT NOT NULL);
    CREATE TABLE batches (batch_id TEXT PRIMARY KEY, device_id TEXT NOT NULL, request_hash TEXT NOT NULL, response BLOB NOT NULL, created_at TEXT NOT NULL);
    ",
    // v2: content-addressed book blobs and resumable uploads (§5.7).
    "
    CREATE TABLE blobs (sha256 TEXT PRIMARY KEY, size INTEGER NOT NULL, object_key TEXT NOT NULL, created_at TEXT NOT NULL);
    CREATE TABLE uploads (sha256 TEXT PRIMARY KEY, total INTEGER NOT NULL, started_at INTEGER NOT NULL);
    ",
];

pub(crate) fn schema_version() -> u32 {
    MIGRATIONS.len() as u32
}

/// Applies every migration above `PRAGMA user_version`, up to `target`. Safe to run repeatedly.
pub(crate) fn migrate(conn: &mut Connection, target: u32) -> rusqlite::Result<u32> {
    let mut current: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    while current < target.min(schema_version()) {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATIONS[current as usize])?;
        if current == 0 {
            let mut id = [0_u8; 8];
            rand::fill(&mut id);
            tx.execute("INSERT INTO meta VALUES ('serverId', ?1)", [format!("srv_{}", hex::encode(id))])?;
            tx.execute("INSERT INTO meta VALUES ('seq', '0')", [])?;
        }
        current += 1;
        tx.pragma_update(None, "user_version", current)?;
        tx.commit()?;
    }
    Ok(current)
}

#[derive(Clone, Default)]
pub(crate) struct Clock(Arc<AtomicI64>);

impl Clock {
    pub(crate) fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc() + time::Duration::seconds(self.0.load(Ordering::Relaxed))
    }
    #[cfg(test)]
    pub(crate) fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::Relaxed);
    }
}

fn stamp(time: OffsetDateTime) -> String {
    time.replace_nanosecond(0).unwrap_or(time).format(&Rfc3339).expect("RFC 3339 timestamp")
}

#[derive(Clone)]
pub(crate) struct Store {
    conn: Arc<Mutex<Connection>>,
    pub(crate) clock: Clock,
    /// Test hook: fail the next push inside its transaction, after every change was applied.
    pub(crate) fail_next_push: Arc<AtomicBool>,
    /// `TAILDRIVE_READER_CRASH_AT=push_before_commit` aborts the process inside the push transaction.
    crash_before_commit: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Device {
    pub id: String,
    pub kind: String,
}

impl Store {
    pub(crate) async fn open(path: PathBuf, crash_before_commit: bool) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let conn = spawn_blocking(move || -> rusqlite::Result<Connection> {
            let mut conn = Connection::open(path)?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "FULL")?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            migrate(&mut conn, schema_version())?;
            Ok(conn)
        })
        .await??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            clock: Clock::default(),
            fail_next_push: Arc::new(AtomicBool::new(false)),
            crash_before_commit,
        })
    }

    pub(crate) async fn run<T, F>(&self, operation: F) -> SyncResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &Store) -> SyncResult<T> + Send + 'static,
    {
        let store = self.clone();
        spawn_blocking(move || {
            let mut conn = store.conn.lock().map_err(|_| SyncError::internal("reader.db lock poisoned"))?;
            operation(&mut conn, &store)
        })
        .await
        .map_err(SyncError::internal)?
    }
}

fn meta(conn: &Connection, key: &str) -> rusqlite::Result<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0))
}

pub(crate) fn server_id(conn: &Connection) -> rusqlite::Result<String> {
    meta(conn, "serverId")
}

fn current_seq(conn: &Connection) -> rusqlite::Result<i64> {
    Ok(meta(conn, "seq")?.parse().unwrap_or(0))
}

fn next_seq(tx: &Transaction<'_>) -> rusqlite::Result<i64> {
    let next = current_seq(tx)? + 1;
    tx.execute("UPDATE meta SET value = ?1 WHERE key = 'seq'", [next.to_string()])?;
    Ok(next)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    rand::fill(buf.as_mut_slice());
    hex::encode(buf)
}

fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn parse_body<'a, T: Deserialize<'a>>(body: &'a [u8], what: &str) -> SyncResult<T> {
    serde_json::from_slice(body).map_err(|error| SyncError::bad(format!("invalid {what}: {error}")))
}

fn raw(text: &str) -> &RawValue {
    serde_json::from_str(text).expect("stored JSON is valid")
}

/// A payload is either JSON null or an object, kept as the exact bytes received.
fn payload_text(raw: &RawValue) -> SyncResult<Option<String>> {
    let text = raw.get();
    if text == "null" {
        return Ok(None);
    }
    if !text.starts_with('{') {
        return Err(SyncError::bad("payload must be an object or null"));
    }
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(SyncError::too_large("payload exceeds 1 MiB"));
    }
    Ok(Some(text.to_owned()))
}

fn is_fingerprint(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_sha256)
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_record_id(id: &str) -> bool {
    // §3.1 / §12.1: 1–512 UTF-8 bytes; reject control characters except tab and NEL,
    // which WordStore.normalize preserves inside words (e.g. "w_ice\tcream").
    (1..=512).contains(&id.len()) && !id.chars().any(|c| c.is_control() && c != '\t' && c != '\u{0085}')
}

// ---------------------------------------------------------------------------------------------
// Devices and pairing (§4, §5.2)

pub(crate) fn health(conn: &Connection) -> SyncResult<Vec<u8>> {
    Ok(wire::canonical_bytes(&json!({
        "ok": true,
        "protocol": PROTOCOL,
        "seq": current_seq(conn)?,
        "serverId": server_id(conn)?,
        "serverVersion": SERVER_VERSION,
    })))
}

pub(crate) fn authenticate(conn: &Connection, credential: &str, now: OffsetDateTime) -> SyncResult<Device> {
    let hash = sha256_hex(credential);
    let row = conn
        .query_row(
            "SELECT id, kind, revoked_at FROM devices WHERE credential_hash = ?1",
            [&hash],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)),
        )
        .optional()?;
    let Some((id, kind, revoked)) = row else {
        return Err(SyncError::unauthorized("unknown credential"));
    };
    if revoked.is_some() {
        return Err(SyncError::unauthorized("device revoked"));
    }
    conn.execute("UPDATE devices SET last_seen_at = ?1 WHERE id = ?2", params![stamp(now), id])?;
    Ok(Device { id, kind })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenRequest {
    pub kind: String,
}

pub(crate) fn issue_token(
    conn: &Connection,
    kind: &str,
    issued_by: &str,
    pair_url: &str,
    now: OffsetDateTime,
) -> SyncResult<Vec<u8>> {
    if kind != "mac" && kind != "phone" {
        return Err(SyncError::bad("kind must be mac or phone"));
    }
    let mut secret = [0_u8; 24];
    rand::fill(&mut secret);
    let token = base32(&secret);
    let expires = now + time::Duration::seconds(PAIRING_TTL_SECONDS);
    conn.execute(
        "INSERT INTO pairing_tokens (token_hash, kind, issued_by, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![sha256_hex(&token), kind, issued_by, expires.unix_timestamp()],
    )?;
    let qr = format!(
        "readersync://pair?v=2&url={}&token={token}&server={}",
        query_escape(pair_url),
        server_id(conn)?
    );
    Ok(wire::canonical_bytes(&json!({ "expiresAt": stamp(expires), "qr": qr, "token": token })))
}

/// Percent-encodes everything outside RFC 3986 unreserved characters plus `:` and `/`,
/// which are legal in a query value. The default URL therefore stays readable.
fn query_escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~:/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RedeemRequest {
    token: String,
    device_id: String,
    name: String,
    platform: String,
    #[serde(default)]
    app_version: Option<String>,
}

pub(crate) fn redeem(conn: &mut Connection, body: &[u8], now: OffsetDateTime) -> SyncResult<Vec<u8>> {
    let request: RedeemRequest = parse_body(body, "redeem body")?;
    if !(1..=128).contains(&request.device_id.len()) || request.device_id.chars().any(char::is_control) {
        return Err(SyncError::bad("invalid deviceId"));
    }
    if !["macos", "android", "ios"].contains(&request.platform.as_str()) {
        return Err(SyncError::bad("platform must be macos, android or ios"));
    }
    let token_hash = sha256_hex(&request.token);
    let tx = conn.transaction()?;
    let token = tx
        .query_row(
            "SELECT kind, expires_at, redeemed_device_id FROM pairing_tokens WHERE token_hash = ?1",
            [&token_hash],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Option<String>>(2)?)),
        )
        .optional()?;
    let Some((kind, expires_at, redeemed_by)) = token else {
        return Err(SyncError::forbidden("unknown pairing token"));
    };
    if expires_at <= now.unix_timestamp() {
        return Err(SyncError::forbidden("pairing token expired"));
    }
    if redeemed_by.as_deref().is_some_and(|device| device != request.device_id) {
        return Err(SyncError::forbidden("pairing token was redeemed by another device"));
    }
    let mut secret = [0_u8; 32];
    rand::fill(&mut secret);
    let credential = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, secret);
    let created = stamp(now);
    // Re-redeeming rotates the credential, so the earlier one stops working (401).
    tx.execute(
        "INSERT INTO devices (id, name, platform, kind, app_version, credential_hash, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, platform = excluded.platform, kind = excluded.kind,
             app_version = excluded.app_version, credential_hash = excluded.credential_hash, revoked_at = NULL",
        params![request.device_id, request.name, request.platform, kind, request.app_version, sha256_hex(&credential), created],
    )?;
    tx.execute(
        "UPDATE pairing_tokens SET redeemed_device_id = ?1, redeemed_at = ?2 WHERE token_hash = ?3",
        params![request.device_id, created, token_hash],
    )?;
    let response = wire::canonical_bytes(&json!({
        "credential": credential, "deviceId": request.device_id, "kind": kind, "serverId": server_id(&tx)?,
    }));
    tx.commit()?;
    Ok(response)
}

pub(crate) fn list_devices(conn: &Connection) -> SyncResult<Vec<u8>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, platform, kind, created_at, last_seen_at, revoked_at FROM devices ORDER BY created_at, id",
    )?;
    let devices = stmt
        .query_map([], |row| {
            Ok(json!({
                "deviceId": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "platform": row.get::<_, String>(2)?,
                "kind": row.get::<_, String>(3)?,
                "createdAt": row.get::<_, String>(4)?,
                "lastSeenAt": row.get::<_, Option<String>>(5)?,
                "revoked": row.get::<_, Option<String>>(6)?.is_some(),
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(wire::canonical_bytes(&json!({ "devices": devices })))
}

pub(crate) fn revoke(conn: &Connection, device_id: &str, now: OffsetDateTime) -> SyncResult<()> {
    let found = conn.execute(
        "UPDATE devices SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2",
        params![stamp(now), device_id],
    )?;
    if found == 0 {
        return Err(SyncError::not_found("no such device"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Records (§5.3–§5.5, §6)

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BatchIn<'a> {
    batch_id: String,
    #[serde(borrow)]
    changes: Vec<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ChangeIn<'a> {
    change_id: String,
    book_id: String,
    kind: String,
    id: String,
    base_version: i64,
    deleted: bool,
    #[serde(default)]
    client_time: Option<String>,
    #[serde(borrow)]
    payload: &'a RawValue,
}

struct Change {
    change_id: String,
    book_id: String,
    kind: String,
    id: String,
    base_version: i64,
    deleted: bool,
    client_time: Option<String>,
    payload: Option<String>,
    hash: String,
}

#[derive(Clone)]
struct Stored {
    version: i64,
    deleted: bool,
    payload: Option<String>,
    device_id: String,
    change_id: String,
    client_time: Option<String>,
    seq: i64,
    conflict_id: Option<String>,
}

fn rejection(change_id: &str, code: &str, message: &str) -> Value {
    json!({ "changeId": change_id, "status": "rejected", "error": { "code": code, "message": message } })
}

fn parse_change(raw: &RawValue) -> SyncResult<Change> {
    let input: ChangeIn<'_> = serde_json::from_str(raw.get()).map_err(|e| SyncError::bad(format!("invalid change: {e}")))?;
    if !(1..=128).contains(&input.change_id.len()) {
        return Err(SyncError::bad("invalid changeId"));
    }
    if !is_fingerprint(&input.book_id) {
        return Err(SyncError::bad("bookId must be sha256:<64 hex>"));
    }
    if !["highlight", "note", "word", "lookup", "position", "book"].contains(&input.kind.as_str()) {
        return Err(SyncError::bad("invalid kind"));
    }
    if !valid_record_id(&input.id) {
        return Err(SyncError::bad("invalid record id"));
    }
    if input.base_version < 0 {
        return Err(SyncError::bad("baseVersion must be >= 0"));
    }
    let payload = payload_text(input.payload)?;
    if !input.deleted && payload.is_none() {
        return Err(SyncError::bad("a live change needs a payload"));
    }
    let value: Value = serde_json::from_str(raw.get()).map_err(|e| SyncError::bad(e.to_string()))?;
    Ok(Change {
        payload,
        hash: sha256_hex(wire::canonical_bytes(&value)),
        change_id: input.change_id,
        book_id: input.book_id,
        kind: input.kind,
        id: input.id,
        base_version: input.base_version,
        deleted: input.deleted,
        client_time: input.client_time,
    })
}

pub(crate) fn push(conn: &mut Connection, store: &Store, device_id: &str, body: &[u8]) -> SyncResult<Vec<u8>> {
    if body.len() > MAX_BATCH_BYTES {
        return Err(SyncError::too_large("batch exceeds 8 MiB"));
    }
    let batch: BatchIn<'_> = parse_body(body, "batch")?;
    if !(1..=128).contains(&batch.batch_id.len()) {
        return Err(SyncError::bad("invalid batchId"));
    }
    if batch.changes.len() > MAX_CHANGES {
        return Err(SyncError::bad("a batch can contain at most 500 changes"));
    }
    let mut changes = Vec::with_capacity(batch.changes.len());
    let mut keys = std::collections::HashSet::new();
    let mut change_ids = std::collections::HashSet::new();
    for raw in &batch.changes {
        let change = parse_change(raw)?;
        if change.kind == "book" {
            return Err(SyncError::new(StatusCode::BAD_REQUEST, "use_register", "book records are written by POST /v1/books"));
        }
        if !keys.insert((change.book_id.clone(), change.kind.clone(), change.id.clone())) {
            return Err(SyncError::bad("at most one change per record per batch"));
        }
        if !change_ids.insert(change.change_id.clone()) {
            return Err(SyncError::bad("duplicate changeId in batch"));
        }
        changes.push(change);
    }
    let whole: Value = serde_json::from_slice(body).map_err(|e| SyncError::bad(e.to_string()))?;
    let request_hash = sha256_hex(wire::canonical_bytes(&whole));
    let now = stamp(store.clock.now());

    let tx = conn.transaction()?;
    if let Some((prior_hash, response)) = tx
        .query_row(
            "SELECT request_hash, response FROM batches WHERE batch_id = ?1",
            [&batch.batch_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
    {
        if prior_hash != request_hash {
            return Err(SyncError::new(StatusCode::UNPROCESSABLE_ENTITY, "change_id_reused", "batchId reused with a different body"));
        }
        return Ok(response);
    }
    let mut results = Vec::with_capacity(changes.len());
    for change in &changes {
        results.push(apply(&tx, device_id, change)?);
    }
    let response = wire::canonical_bytes(&json!({
        "batchId": batch.batch_id, "results": results, "seq": current_seq(&tx)?,
    }));
    tx.execute(
        "INSERT INTO batches (batch_id, device_id, request_hash, response, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![batch.batch_id, device_id, request_hash, response, now],
    )?;
    if store.fail_next_push.swap(false, Ordering::SeqCst) {
        return Err(SyncError::internal("injected failure before commit"));
    }
    if store.crash_before_commit {
        tracing::warn!("TAILDRIVE_READER_CRASH_AT=push_before_commit: aborting inside the push transaction");
        std::process::abort();
    }
    tx.commit()?;
    Ok(response)
}

fn apply(tx: &Transaction<'_>, device_id: &str, change: &Change) -> SyncResult<Value> {
    if let Some((hash, result)) = tx
        .query_row(
            "SELECT request_hash, result FROM applied_changes WHERE change_id = ?1",
            [&change.change_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if hash == change.hash {
            return serde_json::from_str(&result).map_err(SyncError::internal);
        }
        return Ok(rejection(&change.change_id, "change_id_reused", "changeId reused with a different body"));
    }
    let book_live: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM records WHERE kind = 'book' AND book_id = ?1 AND id = ?1 AND deleted = 0)",
            [&change.book_id],
            |row| row.get(0),
        )?;
    let result = if !book_live {
        rejection(&change.change_id, "unknown_book", "book is not registered")
    } else if change.kind == "position" {
        apply_position(tx, device_id, change)?
    } else {
        apply_record(tx, device_id, change)?
    };
    tx.execute(
        "INSERT INTO applied_changes (change_id, device_id, request_hash, result) VALUES (?1, ?2, ?3, ?4)",
        params![change.change_id, device_id, change.hash, String::from_utf8(wire::canonical_bytes(&result)).expect("utf8")],
    )?;
    Ok(result)
}

fn fetch(tx: &Connection, book_id: &str, kind: &str, id: &str) -> rusqlite::Result<Option<Stored>> {
    tx.query_row(
        "SELECT version, deleted, payload, device_id, change_id, client_time, seq, conflict_id
         FROM records WHERE book_id = ?1 AND kind = ?2 AND id = ?3",
        params![book_id, kind, id],
        |row| {
            Ok(Stored {
                version: row.get(0)?,
                deleted: row.get(1)?,
                payload: row.get(2)?,
                device_id: row.get(3)?,
                change_id: row.get(4)?,
                client_time: row.get(5)?,
                seq: row.get(6)?,
                conflict_id: row.get(7)?,
            })
        },
    )
    .optional()
}

fn open_sides(tx: &Connection, conflict_id: Option<&str>) -> rusqlite::Result<Vec<(bool, Option<String>)>> {
    let Some(conflict_id) = conflict_id else { return Ok(Vec::new()) };
    let mut stmt = tx.prepare("SELECT deleted, payload FROM conflict_sides WHERE conflict_id = ?1 ORDER BY side")?;
    stmt.query_map([conflict_id], |row| Ok((row.get(0)?, row.get(1)?)))?.collect()
}

fn head_of(stored: &Option<Stored>) -> Option<Head<'_>> {
    stored.as_ref().map(|s| Head { version: s.version, deleted: s.deleted, payload: s.payload.as_deref() })
}

fn incoming(change: &Change) -> Incoming<'_> {
    Incoming { base_version: change.base_version, deleted: change.deleted, payload: change.payload.as_deref() }
}

fn noop(change: &Change, version: i64, conflict_id: Option<&str>) -> Value {
    let mut result = json!({ "changeId": change.change_id, "status": "noop", "version": version });
    if let Some(conflict_id) = conflict_id {
        result["conflictId"] = json!(conflict_id);
    }
    result
}

fn apply_position(tx: &Transaction<'_>, device_id: &str, change: &Change) -> SyncResult<Value> {
    if let Some(code) = wire::position_rejection(&change.book_id, &change.id, change.payload.as_deref(), device_id, change.deleted) {
        let message = if code == "bad_request" { "positions are never deleted" } else { "position is owned by another device" };
        return Ok(rejection(&change.change_id, code, message));
    }
    let head = fetch(tx, &change.book_id, &change.kind, &change.id)?;
    Ok(match wire::decide_position(head_of(&head), incoming(change)) {
        Decision::Create => write_new(tx, device_id, change, 1, None, None)?,
        Decision::FastForward => {
            let head = head.expect("fast-forward has a head");
            write_new(tx, device_id, change, head.version + 1, None, Some(head))?
        }
        Decision::NoopHead(version) => noop(change, version, None),
        _ => rejection(&change.change_id, "bad_request", "position was not applied"),
    })
}

fn apply_record(tx: &Transaction<'_>, device_id: &str, change: &Change) -> SyncResult<Value> {
    let head = fetch(tx, &change.book_id, &change.kind, &change.id)?;
    let conflict_id = head.as_ref().and_then(|h| h.conflict_id.clone());
    let sides = open_sides(tx, conflict_id.as_deref())?;
    let side_views: Vec<Side<'_>> = sides.iter().map(|(deleted, payload)| Side { deleted: *deleted, payload: payload.as_deref() }).collect();
    let decision = wire::decide(head_of(&head), &side_views, incoming(change));
    Ok(match decision {
        Decision::Create => write_new(tx, device_id, change, 1, None, None)?,
        Decision::FastForward => {
            let head = head.expect("fast-forward has a head");
            // An open conflict stays open across a fast-forward of the head.
            write_new(tx, device_id, change, head.version + 1, conflict_id, Some(head))?
        }
        Decision::NoopHead(version) => noop(change, version, None),
        Decision::NoopConflict => noop(change, head.map_or(0, |h| h.version), conflict_id.as_deref()),
        Decision::OpenConflict => open_conflict(tx, device_id, change, head.expect("conflict has a head"))?,
        Decision::AppendConflict => {
            let head = head.expect("conflict has a head");
            append_side(tx, device_id, change, &head, conflict_id.expect("open conflict"))?
        }
        Decision::Reject(code) => rejection(&change.change_id, code, "baseVersion is not usable"),
    })
}

#[allow(clippy::too_many_arguments)]
fn write_head(
    tx: &Transaction<'_>,
    book_id: &str,
    kind: &str,
    id: &str,
    version: i64,
    deleted: bool,
    payload: Option<&str>,
    device_id: &str,
    change_id: &str,
    client_time: Option<&str>,
    seq: i64,
    conflict_id: Option<&str>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO records (book_id, kind, id, version, deleted, payload, device_id, change_id, client_time, seq, conflict_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (book_id, kind, id) DO UPDATE SET version = excluded.version, deleted = excluded.deleted,
             payload = excluded.payload, device_id = excluded.device_id, change_id = excluded.change_id,
             client_time = excluded.client_time, seq = excluded.seq, conflict_id = excluded.conflict_id",
        params![book_id, kind, id, version, deleted, payload, device_id, change_id, client_time, seq, conflict_id],
    )?;
    tx.execute(
        "INSERT INTO record_versions (book_id, kind, id, version, deleted, payload, device_id, change_id, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![book_id, kind, id, version, deleted, payload, device_id, change_id, seq],
    )?;
    Ok(())
}

fn write_new(
    tx: &Transaction<'_>,
    device_id: &str,
    change: &Change,
    version: i64,
    conflict_id: Option<String>,
    previous: Option<Stored>,
) -> SyncResult<Value> {
    let seq = next_seq(tx)?;
    // A tombstone keeps the last live payload when the delete carries none.
    let payload = if change.deleted {
        change.payload.clone().or_else(|| previous.and_then(|p| p.payload))
    } else {
        change.payload.clone()
    };
    write_head(
        tx, &change.book_id, &change.kind, &change.id, version, change.deleted, payload.as_deref(),
        device_id, &change.change_id, change.client_time.as_deref(), seq, conflict_id.as_deref(),
    )?;
    Ok(json!({ "changeId": change.change_id, "seq": seq, "status": "applied", "version": version }))
}

#[allow(clippy::too_many_arguments)]
fn insert_side(
    tx: &Transaction<'_>,
    conflict_id: &str,
    side: i64,
    change_id: &str,
    device_id: &str,
    version: Option<i64>,
    deleted: bool,
    payload: Option<&str>,
    client_time: Option<&str>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO conflict_sides (conflict_id, side, change_id, device_id, version, deleted, payload, client_time)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![conflict_id, side, change_id, device_id, version, deleted, payload, client_time],
    )?;
    Ok(())
}

fn open_conflict(tx: &Transaction<'_>, device_id: &str, change: &Change, head: Stored) -> SyncResult<Value> {
    if change.base_version > 0 {
        let stored: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM record_versions WHERE book_id = ?1 AND kind = ?2 AND id = ?3 AND version = ?4)",
            params![change.book_id, change.kind, change.id, change.base_version],
            |row| row.get(0),
        )?;
        if !stored {
            return Ok(rejection(&change.change_id, "bad_base", "base version was never stored"));
        }
    }
    let seq = next_seq(tx)?;
    let conflict_id = format!("cf_{}", random_hex(8));
    tx.execute(
        "INSERT INTO conflicts (id, book_id, kind, record_id, base_version, opened_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![conflict_id, change.book_id, change.kind, change.id, change.base_version, seq],
    )?;
    insert_side(tx, &conflict_id, 0, &head.change_id, &head.device_id, Some(head.version), head.deleted, head.payload.as_deref(), head.client_time.as_deref())?;
    insert_side(tx, &conflict_id, 1, &change.change_id, device_id, None, change.deleted, change.payload.as_deref(), change.client_time.as_deref())?;
    tx.execute(
        "UPDATE records SET seq = ?1, conflict_id = ?2 WHERE book_id = ?3 AND kind = ?4 AND id = ?5",
        params![seq, conflict_id, change.book_id, change.kind, change.id],
    )?;
    Ok(json!({ "changeId": change.change_id, "conflictId": conflict_id, "seq": seq, "status": "conflict", "version": head.version }))
}

fn append_side(tx: &Transaction<'_>, device_id: &str, change: &Change, head: &Stored, conflict_id: String) -> SyncResult<Value> {
    let seq = next_seq(tx)?;
    let side: i64 = tx.query_row(
        "SELECT COALESCE(MAX(side), -1) + 1 FROM conflict_sides WHERE conflict_id = ?1",
        [&conflict_id],
        |row| row.get(0),
    )?;
    insert_side(tx, &conflict_id, side, &change.change_id, device_id, None, change.deleted, change.payload.as_deref(), change.client_time.as_deref())?;
    tx.execute(
        "UPDATE records SET seq = ?1 WHERE book_id = ?2 AND kind = ?3 AND id = ?4",
        params![seq, change.book_id, change.kind, change.id],
    )?;
    Ok(json!({ "changeId": change.change_id, "conflictId": conflict_id, "seq": seq, "status": "conflict", "version": head.version }))
}

// ---------------------------------------------------------------------------------------------
// Feed and conflicts

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvelopeOut<'a> {
    book_id: &'a str,
    change_id: &'a str,
    client_time: Option<&'a str>,
    conflict: Option<&'a RawValue>,
    deleted: bool,
    device_id: &'a str,
    id: &'a str,
    kind: &'a str,
    payload: Option<&'a RawValue>,
    seq: i64,
    version: i64,
}

fn envelope(book_id: &str, kind: &str, id: &str, record: &Stored, conflict: Option<&str>) -> String {
    serde_json::to_string(&EnvelopeOut {
        book_id,
        change_id: &record.change_id,
        client_time: record.client_time.as_deref(),
        conflict: conflict.map(raw),
        deleted: record.deleted,
        device_id: &record.device_id,
        id,
        kind,
        payload: record.payload.as_deref().map(raw),
        seq: record.seq,
        version: record.version,
    })
    .expect("envelope serializes")
}

#[derive(Serialize)]
struct BaseOut<'a> {
    deleted: bool,
    payload: Option<&'a RawValue>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SideOut<'a> {
    change_id: &'a str,
    client_time: Option<&'a str>,
    deleted: bool,
    device_id: &'a str,
    payload: Option<&'a RawValue>,
    version: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConflictOut<'a> {
    base: Option<BaseOut<'a>>,
    base_version: i64,
    book_id: &'a str,
    conflict_id: &'a str,
    id: &'a str,
    kind: &'a str,
    opened_seq: i64,
    sides: Vec<SideOut<'a>>,
}

/// The open conflict as JSON, or `None` when it is resolved or absent.
fn conflict_json(conn: &Connection, conflict_id: Option<&str>) -> rusqlite::Result<Option<String>> {
    let Some(conflict_id) = conflict_id else { return Ok(None) };
    let row = conn
        .query_row(
            "SELECT book_id, kind, record_id, base_version, opened_seq FROM conflicts WHERE id = ?1 AND resolved_seq IS NULL",
            [conflict_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?)),
        )
        .optional()?;
    let Some((book_id, kind, record_id, base_version, opened_seq)) = row else { return Ok(None) };
    let base = if base_version == 0 {
        None
    } else {
        conn.query_row(
            "SELECT deleted, payload FROM record_versions WHERE book_id = ?1 AND kind = ?2 AND id = ?3 AND version = ?4",
            params![book_id, kind, record_id, base_version],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?
    };
    let mut stmt = conn.prepare(
        "SELECT change_id, client_time, deleted, device_id, payload, version FROM conflict_sides WHERE conflict_id = ?1 ORDER BY side",
    )?;
    #[allow(clippy::type_complexity)]
    let sides: Vec<(String, Option<String>, bool, String, Option<String>, Option<i64>)> = stmt
        .query_map([conflict_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let out = ConflictOut {
        base: base.as_ref().map(|(deleted, payload)| BaseOut { deleted: *deleted, payload: payload.as_deref().map(raw) }),
        base_version,
        book_id: &book_id,
        conflict_id,
        id: &record_id,
        kind: &kind,
        opened_seq,
        sides: sides
            .iter()
            .map(|(change_id, client_time, deleted, device_id, payload, version)| SideOut {
                change_id,
                client_time: client_time.as_deref(),
                deleted: *deleted,
                device_id,
                payload: payload.as_deref().map(raw),
                version: *version,
            })
            .collect(),
    };
    Ok(Some(serde_json::to_string(&out).expect("conflict serializes")))
}

pub(crate) fn pull(conn: &Connection, since: i64, limit: i64) -> SyncResult<Vec<u8>> {
    if since < 0 || !(1..=1000).contains(&limit) {
        return Err(SyncError::bad("since must be >= 0 and limit 1..=1000"));
    }
    let mut stmt = conn.prepare(
        "SELECT book_id, kind, id, version, deleted, payload, device_id, change_id, client_time, seq, conflict_id
         FROM records WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows: Vec<(String, String, String, Stored)> = stmt
        .query_map(params![since, limit + 1], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                Stored {
                    version: row.get(3)?,
                    deleted: row.get(4)?,
                    payload: row.get(5)?,
                    device_id: row.get(6)?,
                    change_id: row.get(7)?,
                    client_time: row.get(8)?,
                    seq: row.get(9)?,
                    conflict_id: row.get(10)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let has_more = rows.len() as i64 > limit;
    let page = &rows[..rows.len().min(limit as usize)];
    let mut changes = Vec::with_capacity(page.len());
    for (book_id, kind, id, record) in page {
        let conflict = conflict_json(conn, record.conflict_id.as_deref())?;
        changes.push(envelope(book_id, kind, id, record, conflict.as_deref()));
    }
    let cursor = page.last().map_or(since, |(_, _, _, record)| record.seq);
    let changes: Vec<&RawValue> = changes.iter().map(|c| raw(c)).collect();
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Page<'a> {
        changes: Vec<&'a RawValue>,
        cursor: i64,
        has_more: bool,
        server_id: String,
    }
    Ok(serde_json::to_vec(&Page { changes, cursor, has_more, server_id: server_id(conn)? }).expect("page serializes"))
}

pub(crate) fn list_conflicts(conn: &Connection, book_id: Option<&str>) -> SyncResult<Vec<u8>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM conflicts WHERE resolved_seq IS NULL AND (?1 IS NULL OR book_id = ?1) ORDER BY opened_seq",
    )?;
    let ids: Vec<String> = stmt.query_map([book_id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut items = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(conflict) = conflict_json(conn, Some(id))? {
            items.push(conflict);
        }
    }
    #[derive(Serialize)]
    struct Conflicts<'a> {
        conflicts: Vec<&'a RawValue>,
    }
    let conflicts: Vec<&RawValue> = items.iter().map(|c| raw(c)).collect();
    Ok(serde_json::to_vec(&Conflicts { conflicts }).expect("conflicts serialize"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ResolveIn<'a> {
    change_id: String,
    base_version: i64,
    deleted: bool,
    #[serde(borrow)]
    payload: &'a RawValue,
}

pub(crate) fn resolve(conn: &mut Connection, device_id: &str, conflict_id: &str, body: &[u8]) -> SyncResult<Vec<u8>> {
    let request: ResolveIn<'_> = parse_body(body, "resolve body")?;
    if !(1..=128).contains(&request.change_id.len()) {
        return Err(SyncError::bad("invalid changeId"));
    }
    let payload = payload_text(request.payload)?;
    if !request.deleted && payload.is_none() {
        return Err(SyncError::bad("a live resolution needs a payload"));
    }
    let whole: Value = serde_json::from_slice(body).map_err(|e| SyncError::bad(e.to_string()))?;
    let hash = sha256_hex(wire::canonical_bytes(&json!({
        "body": whole,
        "conflictId": conflict_id,
    })));
    let tx = conn.transaction()?;
    if let Some((prior, result)) = tx
        .query_row(
            "SELECT request_hash, result FROM applied_changes WHERE change_id = ?1",
            [&request.change_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if prior != hash {
            return Err(SyncError::new(StatusCode::UNPROCESSABLE_ENTITY, "change_id_reused", "changeId reused with a different body"));
        }
        return Ok(result.into_bytes());
    }
    let conflict = tx
        .query_row(
            "SELECT book_id, kind, record_id, resolved_seq FROM conflicts WHERE id = ?1",
            [conflict_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<i64>>(3)?)),
        )
        .optional()?;
    let Some((book_id, kind, record_id, resolved)) = conflict else {
        return Err(SyncError::not_found("conflict not found"));
    };
    if resolved.is_some() {
        return Err(SyncError::new(StatusCode::CONFLICT, "already_resolved", "conflict already resolved"));
    }
    let head = fetch(&tx, &book_id, &kind, &record_id)?.ok_or_else(|| SyncError::not_found("record not found"))?;
    if request.base_version != head.version {
        return Err(SyncError::new(StatusCode::CONFLICT, "stale_base", "baseVersion is not the current head"));
    }
    let seq = next_seq(&tx)?;
    let version = head.version + 1;
    let stored = if request.deleted { payload.or(head.payload) } else { payload };
    write_head(&tx, &book_id, &kind, &record_id, version, request.deleted, stored.as_deref(), device_id, &request.change_id, None, seq, None)?;
    tx.execute(
        "UPDATE conflicts SET resolved_seq = ?1, resolved_change_id = ?2 WHERE id = ?3",
        params![seq, request.change_id, conflict_id],
    )?;
    let response = wire::canonical_bytes(&json!({ "seq": seq, "status": "applied", "version": version }));
    tx.execute(
        "INSERT INTO applied_changes (change_id, device_id, request_hash, result) VALUES (?1, ?2, ?3, ?4)",
        params![request.change_id, device_id, hash, String::from_utf8(response.clone()).expect("utf8")],
    )?;
    tx.commit()?;
    Ok(response)
}

// ---------------------------------------------------------------------------------------------
// Books (§3.3, §5.6)

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RegisterIn {
    sha256: String,
    size: i64,
    format: String,
    #[serde(default)]
    doc_id: Option<String>,
    title: String,
    filename: String,
}

pub(crate) fn register_book(conn: &mut Connection, device_id: &str, body: &[u8], now: OffsetDateTime) -> SyncResult<Vec<u8>> {
    let request: RegisterIn = parse_body(body, "book registration")?;
    if !is_sha256(&request.sha256) {
        return Err(SyncError::bad("sha256 must be 64 lowercase hex"));
    }
    if request.size < 0 {
        return Err(SyncError::bad("size must be >= 0"));
    }
    if request.format != "pdf" && request.format != "epub" {
        return Err(SyncError::bad("format must be pdf or epub"));
    }
    let doc_id = request.doc_id.filter(|d| !d.is_empty());
    let added = stamp(now);
    let tx = conn.transaction()?;
    // Rule 1: the file is already part of a book.
    if let Some(book_id) = tx
        .query_row("SELECT book_id FROM book_files WHERE sha256 = ?1", [&request.sha256], |row| row.get::<_, String>(0))
        .optional()?
    {
        let record = fetch(&tx, &book_id, "book", &book_id)?.ok_or_else(|| SyncError::internal("book record missing"))?;
        return Ok(register_body("existing", &book_id, &record));
    }
    // Rule 2: a non-empty docId of a live book with the same format: attach.
    if let Some(doc_id) = &doc_id
        && let Some(book_id) = tx
            .query_row(
                "SELECT d.book_id FROM book_doc_ids d JOIN records r ON r.kind = 'book' AND r.book_id = d.book_id AND r.id = d.book_id
                 WHERE d.format = ?1 AND d.doc_id = ?2 AND r.deleted = 0",
                params![request.format, doc_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
    {
        let head = fetch(&tx, &book_id, "book", &book_id)?.ok_or_else(|| SyncError::internal("book record missing"))?;
        let mut payload: Value = serde_json::from_str(head.payload.as_deref().unwrap_or("{}")).map_err(SyncError::internal)?;
        if let Some(files) = payload.get_mut("files").and_then(Value::as_array_mut) {
            files.push(json!({ "addedAt": added, "sha256": request.sha256, "size": request.size }));
        }
        payload["currentSha256"] = json!(request.sha256);
        let bytes = String::from_utf8(wire::canonical_bytes(&payload)).expect("utf8");
        let seq = next_seq(&tx)?;
        let change_id = format!("c_reg_{seq}");
        let version = head.version + 1;
        write_head(&tx, &book_id, "book", &book_id, version, false, Some(&bytes), device_id, &change_id, None, seq, head.conflict_id.as_deref())?;
        tx.execute("INSERT INTO book_files VALUES (?1, ?2, ?3, ?4)", params![request.sha256, book_id, request.size, added])?;
        let record = fetch(&tx, &book_id, "book", &book_id)?.expect("just written");
        tx.commit()?;
        return Ok(register_body("attached", &book_id, &record));
    }
    // Rule 3: create.
    let book_id = format!("sha256:{}", request.sha256);
    let mut payload = json!({
        "currentSha256": request.sha256,
        "extras": {},
        "filename": request.filename,
        "files": [{ "addedAt": added, "sha256": request.sha256, "size": request.size }],
        "format": request.format,
        "title": request.title,
    });
    if let Some(doc_id) = &doc_id {
        payload["docId"] = json!(doc_id);
    }
    let bytes = String::from_utf8(wire::canonical_bytes(&payload)).expect("utf8");
    let seq = next_seq(&tx)?;
    let change_id = format!("c_reg_{seq}");
    write_head(&tx, &book_id, "book", &book_id, 1, false, Some(&bytes), device_id, &change_id, None, seq, None)?;
    tx.execute("INSERT INTO book_files VALUES (?1, ?2, ?3, ?4)", params![request.sha256, book_id, request.size, added])?;
    if let Some(doc_id) = &doc_id {
        tx.execute("INSERT OR IGNORE INTO book_doc_ids VALUES (?1, ?2, ?3)", params![request.format, doc_id, book_id])?;
    }
    let record = fetch(&tx, &book_id, "book", &book_id)?.expect("just written");
    tx.commit()?;
    Ok(register_body("created", &book_id, &record))
}

fn register_body(status: &str, book_id: &str, record: &Stored) -> Vec<u8> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Registered<'a> {
        book: &'a RawValue,
        book_id: &'a str,
        status: &'a str,
    }
    let book = envelope(book_id, "book", book_id, record, None);
    serde_json::to_vec(&Registered { book: raw(&book), book_id, status }).expect("register serializes")
}

pub(crate) fn list_books(conn: &Connection) -> SyncResult<Vec<u8>> {
    let mut stmt = conn.prepare("SELECT id, payload, version, seq FROM records WHERE kind = 'book' AND deleted = 0 ORDER BY seq, id")?;
    let rows: Vec<(String, Option<String>, i64, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut books = Vec::with_capacity(rows.len());
    for (book_id, payload, version, seq) in rows {
        let Some(payload) = payload.and_then(|p| serde_json::from_str::<Value>(&p).ok()) else { continue };
        let current = payload["currentSha256"].as_str().unwrap_or_default().to_owned();
        let mut files = Vec::new();
        let mut size = 0;
        for file in payload["files"].as_array().into_iter().flatten() {
            let sha = file["sha256"].as_str().unwrap_or_default();
            let file_size = file["size"].as_i64().unwrap_or(0);
            if sha == current {
                size = file_size;
            }
            files.push(json!({ "hasBlob": blob_size(conn, sha)?.is_some(), "sha256": sha, "size": file_size }));
        }
        books.push(json!({
            "bookId": book_id,
            "docId": payload.get("docId").cloned().unwrap_or(Value::Null),
            "filename": payload["filename"],
            "files": files,
            "format": payload["format"],
            "hasBlob": blob_size(conn, &current)?.is_some(),
            "seq": seq,
            "sha256": current,
            "size": size,
            "title": payload["title"],
            "version": version,
        }));
    }
    Ok(wire::canonical_bytes(&json!({ "books": books })))
}

// ---------------------------------------------------------------------------------------------
// Blob bookkeeping (§5.7). Bytes live in TailDrive's StorageBackend; reader.db says what is complete.

pub(crate) fn blob_size(conn: &Connection, sha: &str) -> rusqlite::Result<Option<u64>> {
    conn.query_row("SELECT size FROM blobs WHERE sha256 = ?1", [sha], |row| row.get(0)).optional()
}

pub(crate) fn blob_bytes_total(conn: &Connection) -> rusqlite::Result<u64> {
    conn.query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |row| row.get(0))
}

pub(crate) fn insert_blob(conn: &Connection, sha: &str, size: u64, key: &str, now: OffsetDateTime) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO blobs (sha256, size, object_key, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![sha, size, key, stamp(now)],
    )?;
    conn.execute("DELETE FROM uploads WHERE sha256 = ?1", [sha])?;
    Ok(())
}

pub(crate) fn upload_total(conn: &Connection, sha: &str) -> rusqlite::Result<Option<u64>> {
    conn.query_row("SELECT total FROM uploads WHERE sha256 = ?1", [sha], |row| row.get(0)).optional()
}

pub(crate) fn start_upload(conn: &Connection, sha: &str, total: u64, now: OffsetDateTime) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO uploads (sha256, total, started_at) VALUES (?1, ?2, ?3)",
        params![sha, total, now.unix_timestamp()],
    )?;
    Ok(())
}

pub(crate) fn forget_upload(conn: &Connection, sha: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM uploads WHERE sha256 = ?1", [sha])?;
    Ok(())
}

pub(crate) fn uploads(conn: &Connection) -> rusqlite::Result<Vec<(String, u64, i64)>> {
    let mut stmt = conn.prepare("SELECT sha256, total, started_at FROM uploads")?;
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_versioned_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reader.db");
        let mut conn = Connection::open(&path).unwrap();
        assert_eq!(migrate(&mut conn, 1).unwrap(), 1);
        let id = server_id(&conn).unwrap();
        conn.execute("UPDATE meta SET value = '41' WHERE key = 'seq'", []).unwrap();
        assert!(conn.prepare("SELECT 1 FROM blobs").is_err(), "v2 table absent at v1");
        assert_eq!(migrate(&mut conn, schema_version()).unwrap(), 2);
        assert_eq!(migrate(&mut conn, schema_version()).unwrap(), 2, "re-running is a no-op");
        drop(conn);
        let mut conn = Connection::open(&path).unwrap();
        assert_eq!(migrate(&mut conn, schema_version()).unwrap(), 2);
        assert_eq!(server_id(&conn).unwrap(), id, "serverId survives migration");
        assert_eq!(current_seq(&conn).unwrap(), 41, "data survives migration");
        conn.prepare("SELECT sha256, total FROM uploads").unwrap();
    }

    #[test]
    fn base32_matches_rfc4648() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(&[0_u8; 24]).len(), 39);
    }

    #[test]
    fn valid_record_id_accepts_section_12_1_word_ids_and_rejects_other_controls() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/reader-sync/spec/fixtures/word-id/normalize.json"
        );
        let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 30);
        let mut accepted = 0;
        for case in cases {
            let Some(id) = case["wordId"].as_str() else { continue };
            assert!(valid_record_id(id), "fixture wordId rejected for {}: {id:?}", case["name"]);
            accepted += 1;
        }
        assert_eq!(accepted, 28, "two of the 30 cases have null wordId after trim");

        for (label, id) in [
            ("NUL", "w_ice\0cream"),
            ("LF", "w_ice\ncream"),
            ("CR", "w_ice\rcream"),
            ("BEL", "w_ice\u{0007}cream"),
            ("DEL", "w_ice\u{007f}cream"),
            ("C1 PAD", "w_ice\u{0080}cream"),
        ] {
            assert!(!valid_record_id(id), "{label} should be rejected");
        }
        assert!(valid_record_id("w_ice\tcream"));
        assert!(valid_record_id("w_ice\u{0085}cream"));
        assert!(!valid_record_id(""));
        assert!(!valid_record_id(&"a".repeat(513)));
        assert!(valid_record_id(&"a".repeat(512)));
    }
}

//! Stateful + concurrent quota, CAS, resumable-upload, and storage-GC invariant harness for
//! TailDrive's `/v1` HTTP API + SQLite `taildrive.db` + `LocalFsStorage`.
//!
//! Configuration via env vars:
//! - `FORMAL_SEED`  (u64, default `0xD41E_2026_1128_0001`)
//! - `FORMAL_CASES` (usize total operation budget across sequential+concurrent phases, default `200`,
//!   mapped at ~20 ops per phase -> 10 phases in standard mode, 50 phases at `FORMAL_CASES=1000`)

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddr},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future::join_all;
use reqwest::StatusCode;
use rusqlite::Connection;
use taildrive_core::{
    client::{ClientError, TailDriveClient, UploadBody},
    hash::sha256_hex,
    models::{FileItem, FileType, InitUploadRequest, QuotaInfo, QuotaStatus},
    path::parse_path,
};
use taildrive_server::{
    App,
    config::Config,
    storage::{ByteStream, LocalFsStorage, StorageBackend, StorageError},
};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    sync::{Barrier, oneshot},
    task::JoinHandle,
    time::{sleep, timeout},
};

const DEFAULT_SEED: u64 = 0xD41E_2026_1128_0001;
const DEFAULT_TOTAL_OPS: usize = 200;
const OPS_PER_PHASE: usize = 20;
const API_TOKEN: &str = "formal-drive-quota-token";
const QUOTA_LIMIT: u64 = 2048;
const FREE_FLOOR: u64 = 1000;
const HEALTHY_FREE: u64 = 12_000;

fn env_seed() -> u64 {
    std::env::var("FORMAL_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SEED)
}

fn env_phases() -> usize {
    let total_ops = std::env::var("FORMAL_CASES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TOTAL_OPS);
    total_ops.div_ceil(OPS_PER_PHASE).clamp(4, 60)
}

/// Deterministic SplitMix64 PRNG for reproducible runs.
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

    fn payload(&mut self, size: usize) -> Bytes {
        let mut buf = Vec::with_capacity(size);
        let mut fill = self.next_u64();
        for i in 0..size {
            if i % 8 == 0 {
                fill = self.next_u64();
            }
            buf.push(((fill >> ((i % 8) * 8)) & 0xFF) as u8);
        }
        Bytes::from(buf)
    }
}

// =================================================================================================
// Floor-Controlled LocalFs Storage Wrapper
// =================================================================================================

struct FloorControlledStorage {
    local: LocalFsStorage,
    free_bytes: AtomicU64,
    publish_delay_ms: AtomicU64,
}

impl FloorControlledStorage {
    async fn new(dir: &Path, initial_free: u64, publish_delay_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            local: LocalFsStorage::new(dir).await.expect("local storage"),
            free_bytes: AtomicU64::new(initial_free),
            publish_delay_ms: AtomicU64::new(publish_delay_ms),
        })
    }

    fn set_free_bytes(&self, bytes: u64) {
        self.free_bytes.store(bytes, Ordering::SeqCst);
    }
}

#[async_trait]
impl StorageBackend for FloorControlledStorage {
    fn driver_name(&self) -> &'static str {
        self.local.driver_name()
    }

    async fn publish(&self, key: &str, staged: &Path) -> Result<(), StorageError> {
        let delay = self.publish_delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            sleep(Duration::from_millis(delay)).await;
        }
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
// Real TCP Drive Server Wrapper
// =================================================================================================

struct DriveServer {
    addr: SocketAddr,
    dir: TempDir,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl DriveServer {
    async fn start(
        quota_bytes: u64,
        free_floor_bytes: u64,
        custom_storage: Option<Arc<dyn StorageBackend>>,
        dir: TempDir,
    ) -> Self {
        let mut config = Config::new(dir.path());
        config.bind = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        config.api_tokens = vec![API_TOKEN.to_owned()];
        config.quota_bytes = quota_bytes;
        config.free_floor_bytes = free_floor_bytes;

        let listener = TcpListener::bind(config.bind).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = match custom_storage {
            Some(storage) => App::with_storage(config, storage).await.expect("app"),
            None => App::new(config).await.expect("app"),
        };
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            app.serve(listener, async {
                let _ = signal.await;
            })
            .await
        });
        Self {
            addr,
            dir,
            shutdown,
            task,
        }
    }

    fn client(&self) -> TailDriveClient {
        TailDriveClient::new(
            &format!("http://127.0.0.1:{}", self.addr.port()),
            Some(API_TOKEN.to_owned()),
        )
        .expect("typed client")
    }

    /// Gracefully stops Axum and waits for `drive.drain().await` to finish all background GC/rollback tasks.
    async fn stop(self) -> TempDir {
        let _ = self.shutdown.send(());
        timeout(Duration::from_secs(10), self.task)
            .await
            .expect("server stop timeout")
            .expect("server task join")
            .expect("server io ok");
        self.dir
    }
}

// =================================================================================================
// Recursive Listing, Filesystem Walk, SQLite Audit & Invariant Oracle
// =================================================================================================

fn collect_files_recursive(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if entry.file_type().expect("file_type").is_dir() {
            out.extend(collect_files_recursive(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn rel_blob_keys(blobs_dir: &Path) -> BTreeSet<String> {
    collect_files_recursive(blobs_dir)
        .into_iter()
        .map(|p| {
            p.strip_prefix(blobs_dir)
                .expect("under blobs dir")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

async fn list_tree_recursive(client: &TailDriveClient) -> Vec<FileItem> {
    let mut all = Vec::new();
    let mut queue = vec![String::new()];
    while let Some(folder) = queue.pop() {
        let mut cursor: Option<String> = None;
        loop {
            // Use small page limit=3 to continuously exercise cursor pagination too.
            let page = client
                .list_files(&folder, cursor.as_deref(), Some(3))
                .await
                .unwrap_or_else(|e| panic!("list_files({folder:?}) failed: {e}"));
            for item in page.items {
                if item.kind == FileType::Folder {
                    queue.push(item.path.clone());
                }
                all.push(item);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
    all.sort_by(|a, b| a.path.cmp(&b.path));
    all
}

#[derive(Clone, Debug)]
struct DbAudit {
    usage_used_bytes: u64,
    sum_files_bytes: u64,
    live_file_keys: BTreeSet<String>,
    active_part_keys: BTreeSet<String>,
    active_upload_ids: Vec<String>,
    garbage_keys: BTreeSet<String>,
}

fn read_db_audit(db_path: &Path) -> DbAudit {
    let conn = Connection::open(db_path).expect("open taildrive.db");
    let usage_used_bytes: u64 = conn
        .query_row(
            "SELECT used_bytes FROM usage WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .expect("usage");
    let sum_files_bytes: u64 = conn
        .query_row("SELECT COALESCE(SUM(size_bytes), 0) FROM files", [], |r| {
            r.get(0)
        })
        .expect("sum files");

    let mut stmt = conn
        .prepare("SELECT object_key FROM files WHERE object_key IS NOT NULL")
        .unwrap();
    let live_file_keys: BTreeSet<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    let mut stmt = conn.prepare("SELECT object_key FROM upload_parts").unwrap();
    let active_part_keys: BTreeSet<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    let mut stmt = conn.prepare("SELECT id FROM uploads").unwrap();
    let active_upload_ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    let mut stmt = conn.prepare("SELECT object_key FROM garbage").unwrap();
    let garbage_keys: BTreeSet<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    DbAudit {
        usage_used_bytes,
        sum_files_bytes,
        live_file_keys,
        active_part_keys,
        active_upload_ids,
        garbage_keys,
    }
}

#[derive(Clone, Debug)]
struct DriveSnapshot {
    quota: QuotaInfo,
    live_items: Vec<FileItem>,
    downloads: BTreeMap<String, (u64, String)>,
    blob_keys_on_disk: BTreeSet<String>,
    tmp_files_on_disk: Vec<PathBuf>,
    db: DbAudit,
}

/// Pure invariant checker for TailDrive quota, live file integrity, and storage GC (also canary-tested).
fn verify_drive_invariants(
    snap: &DriveSnapshot,
    expect_drained_no_active_uploads: bool,
) -> Result<(), String> {
    // 1. Quota bounds
    if snap.quota.used_bytes > snap.quota.limit_bytes {
        return Err(format!(
            "QUOTA EXCEEDED INVARIANT BROKEN: used_bytes={} > limit_bytes={}",
            snap.quota.used_bytes, snap.quota.limit_bytes
        ));
    }
    let expected_remaining = snap.quota.limit_bytes.saturating_sub(snap.quota.used_bytes);
    if snap.quota.remaining_bytes != expected_remaining {
        return Err(format!(
            "remaining_bytes mismatch: got {}, expected {expected_remaining}",
            snap.quota.remaining_bytes
        ));
    }

    // 2. Tree well-formedness & exact sum of live file sizes
    let mut item_by_path: BTreeMap<&str, &FileItem> = BTreeMap::new();
    let mut sum_live_files = 0_u64;
    let mut live_file_count = 0usize;
    for item in &snap.live_items {
        if item_by_path.insert(item.path.as_str(), item).is_some() {
            return Err(format!(
                "duplicate path in recursive listing: {}",
                item.path
            ));
        }
        match item.kind {
            FileType::File => {
                sum_live_files = sum_live_files.saturating_add(item.size_bytes);
                live_file_count += 1;
            }
            FileType::Folder => {
                if item.size_bytes != 0 {
                    return Err(format!(
                        "folder {} has non-zero size_bytes={}",
                        item.path, item.size_bytes
                    ));
                }
            }
        }
    }
    for item in &snap.live_items {
        let parsed = parse_path(&item.path)
            .map_err(|e| format!("invalid listed path {:?}: {e}", item.path))?;
        for anc in parsed.ancestors() {
            match item_by_path.get(anc.as_str()) {
                Some(parent) if parent.kind == FileType::Folder => {}
                other => {
                    return Err(format!(
                        "listed entry {} missing folder ancestor {} (found {:?})",
                        item.path, anc, other
                    ));
                }
            }
        }
    }

    if snap.quota.used_bytes != sum_live_files {
        return Err(format!(
            "quota.used_bytes={} != sum(live files)={sum_live_files}",
            snap.quota.used_bytes
        ));
    }
    if snap.db.usage_used_bytes != sum_live_files || snap.db.sum_files_bytes != sum_live_files {
        return Err(format!(
            "SQLite usage mismatch: usage={}, sum(files)={}, listed={sum_live_files}",
            snap.db.usage_used_bytes, snap.db.sum_files_bytes
        ));
    }

    // 3. Every live file downloads cleanly with exact matching size and SHA-256
    for item in &snap.live_items {
        if item.kind != FileType::File {
            continue;
        }
        let Some((actual_len, actual_sha)) = snap.downloads.get(&item.path) else {
            return Err(format!(
                "dangling live file {}: missing download",
                item.path
            ));
        };
        if *actual_len != item.size_bytes {
            return Err(format!(
                "live file {} size mismatch: metadata={}, downloaded={actual_len}",
                item.path, item.size_bytes
            ));
        }
        if item.sha256.as_deref() != Some(actual_sha.as_str()) {
            return Err(format!(
                "live file {} SHA-256 mismatch: metadata={:?}, downloaded={actual_sha}",
                item.path, item.sha256
            ));
        }
    }

    // 4. Staging & blob storage accounting
    if !snap.tmp_files_on_disk.is_empty() {
        return Err(format!(
            "leaked staging files in tmp/: {:?}",
            snap.tmp_files_on_disk
        ));
    }
    if snap.db.live_file_keys.len() != live_file_count {
        return Err(format!(
            "DB live object_key count {} != listed live files {live_file_count}",
            snap.db.live_file_keys.len()
        ));
    }

    // Every live file key MUST exist on disk (no dangling/deleted live blobs!).
    for live_key in &snap.db.live_file_keys {
        if !snap.blob_keys_on_disk.contains(live_key) {
            return Err(format!(
                "DANGLING LIVE BLOB: DB references {live_key} which is missing on disk"
            ));
        }
    }

    if expect_drained_no_active_uploads {
        if !snap.db.active_upload_ids.is_empty() || !snap.db.active_part_keys.is_empty() {
            return Err(format!(
                "expected no active uploads after drain, found uploads={:?} parts={:?}",
                snap.db.active_upload_ids, snap.db.active_part_keys
            ));
        }
        if !snap.db.garbage_keys.is_empty() {
            return Err(format!(
                "uncollected garbage entries after drain: {:?}",
                snap.db.garbage_keys
            ));
        }
        if snap.blob_keys_on_disk != snap.db.live_file_keys {
            let orphans: Vec<_> = snap
                .blob_keys_on_disk
                .difference(&snap.db.live_file_keys)
                .collect();
            let missing: Vec<_> = snap
                .db
                .live_file_keys
                .difference(&snap.blob_keys_on_disk)
                .collect();
            return Err(format!(
                "ORPHAN / MISSING BLOBS after drain: orphans={orphans:?}, missing={missing:?}"
            ));
        }
    } else {
        // Between settled waves (while some resumable sessions may still be open):
        // Every blob on disk must be referenced by a live file, an active upload part,
        // or a pending garbage entry (Wait: GC runs synchronously after commit/abort/delete,
        // so garbage should already be empty once the HTTP requests settle!).
        let mut accounted: BTreeSet<String> = snap
            .db
            .live_file_keys
            .union(&snap.db.active_part_keys)
            .cloned()
            .collect();
        accounted.extend(snap.db.garbage_keys.iter().cloned());
        let orphans: Vec<_> = snap.blob_keys_on_disk.difference(&accounted).collect();
        if !orphans.is_empty() {
            return Err(format!(
                "leaked orphan blob files on disk: {orphans:?} (accounted={accounted:?})"
            ));
        }
    }

    Ok(())
}

async fn capture_and_verify(
    client: &TailDriveClient,
    data_dir: &Path,
    expect_no_active_uploads: bool,
    context: &str,
) -> DriveSnapshot {
    // Allow brief asynchronous rollback/tmp removal tasks to finish before strict tmp/orphan check.
    let tmp_dir = data_dir.join("tmp");
    let _ = timeout(Duration::from_secs(2), async {
        while !collect_files_recursive(&tmp_dir).is_empty() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await;

    let health = client
        .health()
        .await
        .unwrap_or_else(|e| panic!("{context}: health failed: {e}"));
    let live_items = list_tree_recursive(client).await;

    let mut downloads = BTreeMap::new();
    for item in &live_items {
        if item.kind != FileType::File {
            continue;
        }
        let resp = client
            .download(&item.path, None)
            .await
            .unwrap_or_else(|e| panic!("{context}: download({}) failed: {e}", item.path));
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.bytes().await.expect("download body");
        downloads.insert(item.path.clone(), (body.len() as u64, sha256_hex(&body)));
    }

    let db_path = data_dir.join("taildrive.db");
    let db = tokio::task::spawn_blocking(move || read_db_audit(&db_path))
        .await
        .expect("db audit");

    let snap = DriveSnapshot {
        quota: health.quota,
        live_items,
        downloads,
        blob_keys_on_disk: rel_blob_keys(&data_dir.join("blobs")),
        tmp_files_on_disk: collect_files_recursive(&tmp_dir),
        db,
    };

    verify_drive_invariants(&snap, expect_no_active_uploads)
        .unwrap_or_else(|err| panic!("DRIVE INVARIANT VIOLATION ({context}): {err}"));

    snap
}

fn assert_quota_in_bounds(quota: &QuotaInfo, context: &str) {
    assert!(
        quota.used_bytes <= quota.limit_bytes,
        "{context}: observed quota.used_bytes={} > limit_bytes={}",
        quota.used_bytes,
        quota.limit_bytes
    );
    assert_eq!(
        quota.remaining_bytes,
        quota.limit_bytes - quota.used_bytes,
        "{context}: remaining_bytes inconsistent"
    );
}

fn is_expected_client_error(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Api { status, .. }
            if matches!(
                *status,
                StatusCode::BAD_REQUEST
                    | StatusCode::NOT_FOUND
                    | StatusCode::CONFLICT
                    | StatusCode::UNPROCESSABLE_ENTITY
                    | StatusCode::INSUFFICIENT_STORAGE
            )
    )
}

#[derive(Clone)]
struct PendingUpload {
    id: String,
    path: String,
    data: Bytes,
    sha256: String,
    part_uploaded: bool,
}

#[derive(Default, Debug)]
struct QuotaStats {
    put_ok: usize,
    put_quota_exceeded: usize,
    put_disk_floor: usize,
    put_if_match_ok: usize,
    put_if_match_conflict: usize,
    resumable_init_ok: usize,
    resumable_part_ok: usize,
    resumable_complete_ok: usize,
    resumable_abort_ok: usize,
    folder_create_ok: usize,
    delete_file_ok: usize,
    delete_non_empty_409: usize,
    delete_recursive_ok: usize,
    get_ok: usize,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drive_stateful_interleaved_phases_and_concurrent_waves() {
    let seed = env_seed();
    let phases = env_phases();
    let mut rng = Rng::new(seed);

    let dir = tempfile::tempdir().expect("tempdir");
    let storage = FloorControlledStorage::new(dir.path(), HEALTHY_FREE, 2).await;
    let server = DriveServer::start(QUOTA_LIMIT, FREE_FLOOR, Some(storage.clone()), dir).await;
    let client = server.client();

    let file_paths: [&str; 8] = [
        "root.txt",
        "a/f1.txt",
        "a/f2.txt",
        "a/sub/f3.bin",
        "b/g1.txt",
        "b/g2.txt",
        "caf\u{00e9}/note.txt",
        "cafe\u{0301}/note.txt", // NFD equivalent to previous path!
    ];
    let folder_paths: [&str; 5] = ["a", "a/sub", "b", "empty_dir", "caf\u{00e9}"];

    let mut pending_uploads: Vec<PendingUpload> = Vec::new();
    let mut stats = QuotaStats::default();

    for phase in 0..phases {
        // ---------------------------------------------------------------------
        // Part A: Sequential interleaved operations
        // ---------------------------------------------------------------------
        for step in 0..10 {
            let op = rng.range(100);
            if op < 30 {
                // Direct PUT (with optional If-Match, random sizes including 0 and quota-busting sizes)
                let path = file_paths[rng.range(file_paths.len())];
                let size = match rng.range(5) {
                    0 => 0,
                    1..=3 => 64 + rng.range(512),
                    _ => 700 + rng.range(1600), // often exceeds remaining or total quota
                };
                let data = rng.payload(size);
                let if_match: Option<String> = if rng.chance(1, 4) {
                    // Fetch current etag if present, or use stale etag
                    let items = list_tree_recursive(&client).await;
                    let canonical = parse_path(path).unwrap().to_string();
                    let cur_etag = items
                        .into_iter()
                        .find(|i| i.path == canonical && i.kind == FileType::File)
                        .and_then(|i| i.etag);
                    if rng.chance(1, 2) {
                        cur_etag.or_else(|| Some("\"stale-etag\"".to_owned()))
                    } else {
                        Some("\"wrong-etag\"".to_owned())
                    }
                } else {
                    None
                };

                match client
                    .put_file(
                        path,
                        UploadBody::from_bytes(data),
                        None,
                        if_match.as_deref(),
                    )
                    .await
                {
                    Ok(resp) => {
                        assert_quota_in_bounds(
                            &resp.quota,
                            &format!("seed={seed} phase={phase} step={step} put"),
                        );
                        stats.put_ok += 1;
                        if if_match.is_some() {
                            stats.put_if_match_ok += 1;
                        }
                    }
                    Err(err) => {
                        assert!(
                            is_expected_client_error(&err),
                            "unexpected put error: {err}"
                        );
                        if let ClientError::Api { body, .. } = &err {
                            match body.error.code.as_str() {
                                "ERR_QUOTA_EXCEEDED" => stats.put_quota_exceeded += 1,
                                "ERR_DISK_FLOOR" => stats.put_disk_floor += 1,
                                "ERR_CONFLICT" if if_match.is_some() => {
                                    stats.put_if_match_conflict += 1
                                }
                                _ => {}
                            }
                        }
                    }
                }
            } else if op < 58 {
                // Resumable upload lifecycle: init -> put_part (possibly twice!) -> complete OR abort
                let sub = rng.range(4);
                if sub == 0 || pending_uploads.is_empty() {
                    // Init new resumable upload
                    let path = file_paths[rng.range(file_paths.len())];
                    let size = 32 + rng.range(600);
                    let data = rng.payload(size);
                    let sha = sha256_hex(&data);
                    let req = InitUploadRequest {
                        path: path.to_owned(),
                        size_bytes: size as u64,
                        mime: Some("application/octet-stream".to_owned()),
                    };
                    match client.init_upload(&req).await {
                        Ok(init) => {
                            stats.resumable_init_ok += 1;
                            // Test idempotent re-init returns exact same upload_id
                            let again = client.init_upload(&req).await.expect("idempotent re-init");
                            assert_eq!(again, init);
                            if !pending_uploads.iter().any(|u| u.id == init.upload_id) {
                                pending_uploads.push(PendingUpload {
                                    id: init.upload_id,
                                    path: parse_path(path).unwrap().to_string(),
                                    data,
                                    sha256: sha,
                                    part_uploaded: false,
                                });
                            }
                        }
                        Err(err) => {
                            assert!(
                                is_expected_client_error(&err),
                                "unexpected init error: {err}"
                            );
                        }
                    }
                } else if sub == 1 {
                    // Put part 1 (or re-upload part 1 to exercise replacing an existing part blob!)
                    let idx = rng.range(pending_uploads.len());
                    let u = &mut pending_uploads[idx];
                    match client
                        .put_part(&u.id, 1, UploadBody::from_bytes(u.data.clone()))
                        .await
                    {
                        Ok(()) => {
                            u.part_uploaded = true;
                            stats.resumable_part_ok += 1;
                        }
                        Err(err) => {
                            assert!(
                                is_expected_client_error(&err),
                                "unexpected put_part error: {err}"
                            );
                        }
                    }
                } else if sub == 2 {
                    // Complete an upload (upload its part first if not yet uploaded)
                    let idx = rng.range(pending_uploads.len());
                    let u = pending_uploads.remove(idx);
                    if !u.part_uploaded {
                        let _ = client
                            .put_part(&u.id, 1, UploadBody::from_bytes(u.data.clone()))
                            .await;
                    }
                    match client.complete_upload(&u.id, &u.sha256, None).await {
                        Ok(resp) => {
                            assert_quota_in_bounds(
                                &resp.quota,
                                &format!("seed={seed} phase={phase} complete"),
                            );
                            assert_eq!(resp.item.path, u.path);
                            assert_eq!(resp.item.sha256.as_deref(), Some(u.sha256.as_str()));
                            stats.resumable_complete_ok += 1;
                        }
                        Err(err) => {
                            assert!(
                                is_expected_client_error(&err),
                                "unexpected complete error: {err}"
                            );
                            // If complete failed (e.g. quota or conflict), abort cleanly if still present
                            let _ = client.abort_upload(&u.id).await;
                        }
                    }
                } else {
                    // Abort an active upload (even after uploading part!) -> must free pending quota & GC part blob
                    let idx = rng.range(pending_uploads.len());
                    let u = pending_uploads.remove(idx);
                    match client.abort_upload(&u.id).await {
                        Ok(()) => stats.resumable_abort_ok += 1,
                        Err(err) => assert!(
                            is_expected_client_error(&err),
                            "unexpected abort error: {err}"
                        ),
                    }
                }
            } else if op < 70 {
                // Create folder
                let folder = folder_paths[rng.range(folder_paths.len())];
                match client.create_folder(folder).await {
                    Ok(item) => {
                        assert_eq!(item.kind, FileType::Folder);
                        stats.folder_create_ok += 1;
                    }
                    Err(err) => assert!(
                        is_expected_client_error(&err),
                        "unexpected folder error: {err}"
                    ),
                }
            } else if op < 90 {
                // DELETE (file, empty folder, non-empty folder 409, or recursive folder delete)
                let del_folder = rng.chance(1, 2);
                if del_folder {
                    let folder = folder_paths[rng.range(folder_paths.len())];
                    let recursive = rng.chance(1, 2);
                    match client.delete_file(folder, recursive).await {
                        Ok(resp) => {
                            assert_quota_in_bounds(
                                &resp.quota,
                                &format!("seed={seed} phase={phase} del_folder"),
                            );
                            if recursive {
                                stats.delete_recursive_ok += 1;
                            } else {
                                stats.delete_file_ok += 1;
                            }
                        }
                        Err(err) => {
                            assert!(
                                is_expected_client_error(&err),
                                "unexpected delete folder error: {err}"
                            );
                            if let ClientError::Api { status, body } = &err
                                && *status == StatusCode::CONFLICT
                                && body.error.code == "ERR_NOT_EMPTY"
                            {
                                assert!(
                                    !recursive,
                                    "recursive delete must never return ERR_NOT_EMPTY"
                                );
                                stats.delete_non_empty_409 += 1;
                            }
                        }
                    }
                } else {
                    let path = file_paths[rng.range(file_paths.len())];
                    match client.delete_file(path, false).await {
                        Ok(resp) => {
                            assert_quota_in_bounds(
                                &resp.quota,
                                &format!("seed={seed} phase={phase} del_file"),
                            );
                            stats.delete_file_ok += 1;
                        }
                        Err(err) => assert!(
                            is_expected_client_error(&err),
                            "unexpected delete file error: {err}"
                        ),
                    }
                }
            } else {
                // GET read of an existing file
                let path = file_paths[rng.range(file_paths.len())];
                match client.download(path, None).await {
                    Ok(resp) => {
                        assert_eq!(resp.status(), StatusCode::OK);
                        let expected_sha = resp
                            .headers()
                            .get("x-content-sha256")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let body = resp.bytes().await.expect("body");
                        assert_eq!(sha256_hex(&body), expected_sha);
                        stats.get_ok += 1;
                    }
                    Err(err) => assert!(
                        is_expected_client_error(&err),
                        "unexpected download error: {err}"
                    ),
                }
            }
        }

        // Verify ALL invariants after sequential phase settles!
        capture_and_verify(
            &client,
            server.dir.path(),
            false,
            &format!("seed={seed} phase={phase} post-sequential"),
        )
        .await;

        // ---------------------------------------------------------------------
        // Part B: Concurrent wave (10 barrier-synchronized tasks racing on quota,
        // CAS If-Match, resumable uploads, deletes, and concurrent GET reads!)
        // ---------------------------------------------------------------------
        // Occasionally dip measured free disk near FREE_FLOOR during a wave to exercise ERR_DISK_FLOOR!
        let dip_disk = phase % 3 == 1;
        if dip_disk {
            storage.set_free_bytes(FREE_FLOOR + 250);
        }

        // Seed or pick a hot file for an If-Match CAS race inside this wave
        let hot_etag = match client
            .put_file(
                "a/f1.txt",
                UploadBody::from_bytes(Bytes::from_static(b"cas-base")),
                None,
                None,
            )
            .await
        {
            Ok(resp) => {
                assert_quota_in_bounds(&resp.quota, "pre-wave hot seed");
                resp.item.etag
            }
            Err(_) => None,
        };

        let task_count = 10usize;
        let barrier = Arc::new(Barrier::new(task_count));
        let mut wave_specs: Vec<(usize, &'static str, Bytes)> = Vec::with_capacity(task_count);
        for t in 0..task_count {
            let p = file_paths[rng.range(file_paths.len())];
            let sz = 128 + rng.range(640);
            wave_specs.push((t, p, rng.payload(sz)));
        }

        let wave_results = join_all(wave_specs.into_iter().map(|(t, path, data)| {
            let client = client.clone();
            let barrier = barrier.clone();
            let hot_etag = hot_etag.clone();
            async move {
                barrier.wait().await;
                match t {
                    // Tasks 0..3: concurrent If-Match CAS racers on "a/f1.txt" with the SAME etag
                    0..=2 => {
                        let res = client
                            .put_file(
                                "a/f1.txt",
                                UploadBody::from_bytes(data),
                                None,
                                hot_etag.as_deref(),
                            )
                            .await;
                        match &res {
                            Ok(u) => assert_quota_in_bounds(&u.quota, "wave cas put"),
                            Err(e) => assert!(
                                is_expected_client_error(e),
                                "unexpected wave cas error: {e}"
                            ),
                        }
                        ("cas", res.is_ok())
                    }
                    // Tasks 3..5: concurrent direct PUTs competing for tight quota
                    3..=4 => {
                        let res = client
                            .put_file(path, UploadBody::from_bytes(data), None, None)
                            .await;
                        match &res {
                            Ok(u) => assert_quota_in_bounds(&u.quota, "wave put"),
                            Err(e) => assert!(
                                is_expected_client_error(e),
                                "unexpected wave put error: {e}"
                            ),
                        }
                        ("put", res.is_ok())
                    }
                    // Tasks 5..6: concurrent resumable upload -> part -> complete OR abort
                    5..=6 => {
                        let sha = sha256_hex(&data);
                        let req = InitUploadRequest {
                            path: format!("b/wave_{t}.bin"),
                            size_bytes: data.len() as u64,
                            mime: None,
                        };
                        if let Ok(init) = client.init_upload(&req).await {
                            let _ = client
                                .put_part(&init.upload_id, 1, UploadBody::from_bytes(data))
                                .await;
                            if t == 5 {
                                match client.complete_upload(&init.upload_id, &sha, None).await {
                                    Ok(u) => {
                                        assert_quota_in_bounds(&u.quota, "wave resumable complete")
                                    }
                                    Err(e) => {
                                        assert!(is_expected_client_error(&e));
                                        let _ = client.abort_upload(&init.upload_id).await;
                                    }
                                }
                            } else {
                                let _ = client.abort_upload(&init.upload_id).await;
                            }
                        }
                        ("resumable", true)
                    }
                    // Tasks 7..8: concurrent deletes (file & non-empty folder check)
                    7 => {
                        match client.delete_file(path, false).await {
                            Ok(d) => assert_quota_in_bounds(&d.quota, "wave delete"),
                            Err(e) => assert!(is_expected_client_error(&e)),
                        }
                        ("delete", true)
                    }
                    8 => {
                        match client.delete_file("b", false).await {
                            Ok(d) => assert_quota_in_bounds(&d.quota, "wave del folder"),
                            Err(e) => assert!(is_expected_client_error(&e)),
                        }
                        ("del_folder", true)
                    }
                    // Task 9: concurrent GET racing with writers & deleters
                    _ => {
                        match client.download("a/f1.txt", None).await {
                            Ok(resp) => {
                                assert_eq!(resp.status(), StatusCode::OK);
                                let hdr_sha = resp
                                    .headers()
                                    .get("x-content-sha256")
                                    .unwrap()
                                    .to_str()
                                    .unwrap()
                                    .to_owned();
                                let body = resp.bytes().await.expect("concurrent download body");
                                assert_eq!(
                                    sha256_hex(&body),
                                    hdr_sha,
                                    "torn or corrupted blob during concurrent GET!"
                                );
                            }
                            Err(e) => assert!(is_expected_client_error(&e)),
                        }
                        ("get", true)
                    }
                }
            }
        }))
        .await;

        if hot_etag.is_some() {
            let cas_winners = wave_results
                .iter()
                .filter(|(kind, ok)| *kind == "cas" && *ok)
                .count();
            assert!(
                cas_winners <= 1,
                "seed={seed} phase={phase}: If-Match CAS allowed {cas_winners} concurrent winners!"
            );
        }

        if dip_disk {
            storage.set_free_bytes(HEALTHY_FREE);
        }

        // Verify ALL invariants after concurrent wave settles!
        capture_and_verify(
            &client,
            server.dir.path(),
            false,
            &format!("seed={seed} phase={phase} post-wave"),
        )
        .await;
    }

    // Drain any still-open resumable uploads tracked by DB and verify zero leaks before & after stop!
    let db_path = server.dir.path().join("taildrive.db");
    let leftover_uploads =
        tokio::task::spawn_blocking(move || read_db_audit(&db_path).active_upload_ids)
            .await
            .expect("read leftover uploads");
    for uid in leftover_uploads {
        let _ = client.abort_upload(&uid).await;
    }

    let pre_stop_snap = capture_and_verify(
        &client,
        server.dir.path(),
        true,
        &format!("seed={seed} final pre-stop"),
    )
    .await;

    // Stop server (which runs `drive.drain().await`) and verify post-drain SQLite + blob directory!
    let dir = server.stop().await;
    let post_db = read_db_audit(&dir.path().join("taildrive.db"));
    let post_blobs = rel_blob_keys(&dir.path().join("blobs"));
    let post_tmp = collect_files_recursive(&dir.path().join("tmp"));

    assert!(
        post_tmp.is_empty(),
        "leaked tmp files after drain: {post_tmp:?}"
    );
    assert!(
        post_db.garbage_keys.is_empty(),
        "leaked garbage after drain: {:?}",
        post_db.garbage_keys
    );
    assert!(post_db.active_upload_ids.is_empty());
    assert!(post_db.active_part_keys.is_empty());
    assert_eq!(post_db.usage_used_bytes, post_db.sum_files_bytes);
    assert!(post_db.usage_used_bytes <= QUOTA_LIMIT);
    assert_eq!(
        post_blobs, post_db.live_file_keys,
        "orphan blobs remain on disk after drain!"
    );
    assert_eq!(post_blobs.len(), pre_stop_snap.db.live_file_keys.len());

    // Sanity check that key operations actually exercised (for default >= 8 phases)
    if phases >= 8 {
        assert!(stats.put_ok > 0, "no put_ok: {stats:?}");
        assert!(
            stats.resumable_init_ok > 0,
            "no resumable_init_ok: {stats:?}"
        );
        assert!(
            stats.delete_file_ok + stats.delete_recursive_ok > 0,
            "no deletes: {stats:?}"
        );
    }
}

/// High-contention concurrent If-Match CAS + tight quota test using raw unmodified `LocalFsStorage`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drive_if_match_cas_and_raw_local_fs_quota_waves() {
    let seed = env_seed().wrapping_add(0xCAFE);
    let mut rng = Rng::new(seed);
    let dir = tempfile::tempdir().expect("tempdir");
    let server = DriveServer::start(1024, 0, None, dir).await;
    let client = server.client();

    let mut cur = client
        .put_file(
            "hot/cas.bin",
            UploadBody::from_bytes(Bytes::from(vec![0xAA; 200])),
            None,
            None,
        )
        .await
        .expect("initial put");
    assert_quota_in_bounds(&cur.quota, "initial cas put");

    for wave in 0..8usize {
        let etag = cur.item.etag.clone().expect("etag");
        let prev_version = cur.item.version;
        let racers = 12usize;
        let barrier = Arc::new(Barrier::new(racers));
        let payloads: Vec<Bytes> = (0..racers)
            .map(|i| {
                // Mix sizes that fit (<= 1024) and sizes that exceed quota (> 1024)
                let sz = if i % 4 == 3 {
                    1200
                } else {
                    100 + rng.range(700)
                };
                rng.payload(sz)
            })
            .collect();

        let results = join_all(payloads.into_iter().enumerate().map(|(i, data)| {
            let client = client.clone();
            let barrier = barrier.clone();
            let etag = etag.clone();
            async move {
                barrier.wait().await;
                if i % 3 == 0 {
                    // Resumable upload completing with If-Match against direct PUT racers!
                    let sha = sha256_hex(&data);
                    let req = InitUploadRequest {
                        path: format!("hot/side_{wave}_{i}.bin"),
                        size_bytes: data.len() as u64,
                        mime: None,
                    };
                    if let Ok(init) = client.init_upload(&req).await {
                        if client
                            .put_part(&init.upload_id, 1, UploadBody::from_bytes(data))
                            .await
                            .is_ok()
                        {
                            match client.complete_upload(&init.upload_id, &sha, None).await {
                                Ok(u) => assert_quota_in_bounds(&u.quota, "side resumable"),
                                Err(e) => {
                                    assert!(is_expected_client_error(&e));
                                    let _ = client.abort_upload(&init.upload_id).await;
                                }
                            }
                        } else {
                            let _ = client.abort_upload(&init.upload_id).await;
                        }
                    }
                    None
                } else {
                    match client
                        .put_file(
                            "hot/cas.bin",
                            UploadBody::from_bytes(data),
                            None,
                            Some(&etag),
                        )
                        .await
                    {
                        Ok(u) => {
                            assert_quota_in_bounds(&u.quota, "cas winner");
                            Some(u)
                        }
                        Err(err) => {
                            assert!(is_expected_client_error(&err), "unexpected cas err: {err}");
                            None
                        }
                    }
                }
            }
        }))
        .await;

        let winners: Vec<_> = results.into_iter().flatten().collect();
        assert!(
            winners.len() <= 1,
            "seed={seed} wave={wave}: {} CAS racers won with the same etag {etag}!",
            winners.len()
        );
        if let Some(winner) = winners.into_iter().next() {
            assert_eq!(winner.item.version, prev_version + 1);
            cur = winner;
        }

        // Delete side files to reclaim room for subsequent waves & exercise GC under churn
        let items = list_tree_recursive(&client).await;
        for item in items {
            if item.kind == FileType::File && item.path.starts_with("hot/side_") {
                let d = client
                    .delete_file(&item.path, false)
                    .await
                    .expect("delete side");
                assert_quota_in_bounds(&d.quota, "delete side");
            }
        }

        // Concurrent race on a SINGLE resumable upload_id: two competing put_part(1) overwrites
        // racing with complete_upload and abort_upload simultaneously!
        let race_data_a = rng.payload(180);
        let race_data_b = rng.payload(180);
        let race_sha_a = sha256_hex(&race_data_a);
        let init_req = InitUploadRequest {
            path: format!("hot/race_{wave}.bin"),
            size_bytes: 180,
            mime: None,
        };
        if let Ok(init) = client.init_upload(&init_req).await {
            let uid = init.upload_id;
            let b = Arc::new(Barrier::new(4));
            let c0 = client.clone();
            let c1 = client.clone();
            let c2 = client.clone();
            let c3 = client.clone();
            let (u0, u1, u2, u3) = (uid.clone(), uid.clone(), uid.clone(), uid.clone());
            let (b0, b1, b2, b3) = (b.clone(), b.clone(), b.clone(), b.clone());
            let _ = tokio::join!(
                async move {
                    b0.wait().await;
                    let _ = c0
                        .put_part(&u0, 1, UploadBody::from_bytes(race_data_a))
                        .await;
                },
                async move {
                    b1.wait().await;
                    let _ = c1
                        .put_part(&u1, 1, UploadBody::from_bytes(race_data_b))
                        .await;
                },
                async move {
                    b2.wait().await;
                    if let Ok(u) = c2.complete_upload(&u2, &race_sha_a, None).await {
                        assert_quota_in_bounds(&u.quota, "same-upload race complete");
                    }
                },
                async move {
                    b3.wait().await;
                    let _ = c3.abort_upload(&u3).await;
                },
            );
            // Clean up if upload remained open (e.g., abort ran after failed complete or vice versa)
            let _ = client.abort_upload(&uid).await;
            let _ = client
                .delete_file(&format!("hot/race_{wave}.bin"), false)
                .await;
        }

        capture_and_verify(
            &client,
            server.dir.path(),
            true,
            &format!("raw-localfs seed={seed} wave={wave}"),
        )
        .await;
    }

    let dir = server.stop().await;
    let post_db = read_db_audit(&dir.path().join("taildrive.db"));
    let post_blobs = rel_blob_keys(&dir.path().join("blobs"));
    assert!(post_db.garbage_keys.is_empty());
    assert_eq!(post_blobs, post_db.live_file_keys);
    assert!(collect_files_recursive(&dir.path().join("tmp")).is_empty());
}

/// Canary self-test confirming `verify_drive_invariants` fails loudly on every class of violation.
#[test]
fn canary_drive_invariant_oracle_catches_violations() {
    let sha = sha256_hex(b"hello");
    let mut downloads = BTreeMap::new();
    downloads.insert("a/f.txt".to_owned(), (5, sha.clone()));
    let mut live_keys = BTreeSet::new();
    live_keys.insert("2c/b_1".to_owned());

    let good = DriveSnapshot {
        quota: QuotaInfo {
            used_bytes: 5,
            limit_bytes: 100,
            remaining_bytes: 95,
            disk_free_bytes: Some(5000),
            disk_floor_bytes: 1000,
            status: QuotaStatus::Ok,
        },
        live_items: vec![
            FileItem {
                id: "d_1".into(),
                name: "a".into(),
                path: "a".into(),
                kind: FileType::Folder,
                size_bytes: 0,
                mime: None,
                sha256: None,
                version: 1,
                etag: None,
                updated_at: "2026-11-28T00:00:00Z".into(),
            },
            FileItem {
                id: "f_1".into(),
                name: "f.txt".into(),
                path: "a/f.txt".into(),
                kind: FileType::File,
                size_bytes: 5,
                mime: Some("text/plain".into()),
                sha256: Some(sha.clone()),
                version: 1,
                etag: Some(format!("\"{sha}\"")),
                updated_at: "2026-11-28T00:00:00Z".into(),
            },
        ],
        downloads,
        blob_keys_on_disk: live_keys.clone(),
        tmp_files_on_disk: vec![],
        db: DbAudit {
            usage_used_bytes: 5,
            sum_files_bytes: 5,
            live_file_keys: live_keys,
            active_part_keys: BTreeSet::new(),
            active_upload_ids: vec![],
            garbage_keys: BTreeSet::new(),
        },
    };

    assert_eq!(verify_drive_invariants(&good, true), Ok(()));

    // Canary 1: quota.used_bytes > quota.limit_bytes
    let mut bad1 = good.clone();
    bad1.quota.used_bytes = 101;
    assert!(verify_drive_invariants(&bad1, true).is_err());

    // Canary 2: quota.used_bytes != sum(live files)
    let mut bad2 = good.clone();
    bad2.quota.used_bytes = 10;
    bad2.quota.remaining_bytes = 90;
    assert!(verify_drive_invariants(&bad2, true).is_err());

    // Canary 3: corrupted blob SHA-256
    let mut bad3 = good.clone();
    bad3.downloads
        .insert("a/f.txt".to_owned(), (5, sha256_hex(b"corrupt")));
    assert!(verify_drive_invariants(&bad3, true).is_err());

    // Canary 4: dangling live blob missing from disk
    let mut bad4 = good.clone();
    bad4.blob_keys_on_disk.clear();
    assert!(verify_drive_invariants(&bad4, true).is_err());

    // Canary 5: leaked orphan blob on disk after drain
    let mut bad5 = good.clone();
    bad5.blob_keys_on_disk.insert("ff/b_orphan".to_owned());
    assert!(verify_drive_invariants(&bad5, true).is_err());

    // Canary 6: leaked tmp staging file
    let mut bad6 = good.clone();
    bad6.tmp_files_on_disk.push(PathBuf::from("/tmp/leaked"));
    assert!(verify_drive_invariants(&bad6, true).is_err());
}

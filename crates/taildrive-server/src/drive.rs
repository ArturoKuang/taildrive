use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::http::{HeaderMap, StatusCode};
use futures_util::{StreamExt, TryStreamExt, stream};
use rusqlite::Connection;
use serde_json::json;
use taildrive_core::{
    models::{DeleteResponse, InitUploadResponse, QuotaInfo, QuotaStatus, UploadResponse},
    path::DrivePath,
};
use tokio::{
    fs,
    sync::{Mutex, RwLock},
};
use tokio_util::task::TaskTracker;

use crate::{
    config::Config,
    db::{self, Db, Entry, StartupResult, Upload, WriteFile},
    error::{ApiError, ApiResult},
    storage::{ByteStream, Spool, StagedFile, StorageBackend, StorageError, free_on_disk},
};

const PART_SIZE: u64 = 8 * 1024 * 1024;
const UPLOAD_LOCKS: usize = 64;

pub(crate) struct Drive {
    pub(crate) db: Db,
    pub(crate) storage: Arc<dyn StorageBackend>,
    spool: Spool,
    spool_dir: PathBuf,
    quota_limit: u64,
    floor: u64,
    reservations: Mutex<Vec<Weak<Reservation>>>,
    upload_locks: [Arc<RwLock<()>>; UPLOAD_LOCKS],
    garbage_lock: Mutex<()>,
    tasks: TaskTracker,
}

struct Reservation {
    quota: u64,
    disk_remaining: AtomicU64,
}

#[derive(Default)]
struct Reserved {
    quota: u64,
    disk: u64,
}

impl Reserved {
    fn collect(
        reservations: &mut Vec<Weak<Reservation>>,
        exclude: Option<&Arc<Reservation>>,
    ) -> Self {
        let mut total = Self::default();
        reservations.retain(|weak| {
            let Some(reservation) = weak.upgrade() else {
                return false;
            };
            if !exclude.is_some_and(|excluded| Arc::ptr_eq(excluded, &reservation)) {
                total.quota = total.quota.saturating_add(reservation.quota);
                total.disk = total
                    .disk
                    .saturating_add(reservation.disk_remaining.load(Ordering::Relaxed));
            }
            true
        });
        total
    }
}

#[derive(Clone, Copy)]
struct Disk {
    reported: Option<u64>,
    spool_free: u64,
}

impl Drive {
    pub(crate) async fn new(
        config: &Config,
        storage: Arc<dyn StorageBackend>,
    ) -> StartupResult<Arc<Self>> {
        let spool_dir = config.data_dir.join("tmp");
        fs::create_dir_all(&spool_dir).await?;
        let db = Db::open(config.data_dir.join("taildrive.db")).await?;
        let tasks = TaskTracker::new();
        let drive = Arc::new(Self {
            db,
            storage,
            spool: Spool::new(spool_dir.clone(), tasks.clone()),
            spool_dir,
            quota_limit: config.quota_bytes,
            floor: config.free_floor_bytes,
            reservations: Mutex::new(Vec::new()),
            upload_locks: std::array::from_fn(|_| Arc::new(RwLock::new(()))),
            garbage_lock: Mutex::new(()),
            tasks,
        });
        drive.collect_garbage().await;
        Ok(drive)
    }

    pub(crate) async fn drain(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub(crate) async fn quota(&self) -> ApiResult<QuotaInfo> {
        let (used, free) = tokio::try_join!(self.db.run(|conn| db::used_bytes(conn)), self.disk())?;
        Ok(self.quota_info(used, free))
    }

    fn quota_info(&self, used: u64, disk: Disk) -> QuotaInfo {
        let ratio = u128::from(used) * 100;
        let limit = u128::from(self.quota_limit);
        let status = if ratio >= limit * 95 || disk.spool_free < self.floor {
            QuotaStatus::Full
        } else if ratio >= limit * 80 {
            QuotaStatus::Warn
        } else {
            QuotaStatus::Ok
        };
        QuotaInfo {
            used_bytes: used,
            limit_bytes: self.quota_limit,
            remaining_bytes: self.quota_limit.saturating_sub(used),
            disk_free_bytes: disk.reported,
            disk_floor_bytes: self.floor,
            status,
        }
    }

    async fn disk(&self) -> ApiResult<Disk> {
        let reported = self.storage.disk_free_bytes().await?;
        let spool_free = match reported {
            Some(free) => free,
            None => free_on_disk(self.spool_dir.clone()).await?,
        };
        Ok(Disk {
            reported,
            spool_free,
        })
    }

    fn check_disk(&self, free: u64, reserved: u64, requested: u64) -> ApiResult<()> {
        if self
            .floor
            .checked_add(reserved)
            .and_then(|n| n.checked_add(requested))
            .is_none_or(|needed| needed > free)
        {
            return Err(ApiError::new(StatusCode::INSUFFICIENT_STORAGE, "ERR_DISK_FLOOR",
                "upload would push the disk below its free-space floor").details(json!({
                    "requested_bytes": requested, "disk_free_bytes": free, "disk_floor_bytes": self.floor,
                })));
        }
        Ok(())
    }

    fn check_quota(&self, used: u64, promised: u64, growth: u64) -> ApiResult<()> {
        if used
            .checked_add(promised)
            .and_then(|n| n.checked_add(growth))
            .is_none_or(|total| total > self.quota_limit)
        {
            return Err(quota_exceeded(self.quota_limit, used, promised, growth));
        }
        Ok(())
    }

    async fn reserve_file(
        &self,
        file: &WriteFile,
        upload_id: Option<&str>,
    ) -> ApiResult<Arc<Reservation>> {
        let mut slots = self.reservations.lock().await;
        let reserved = Reserved::collect(&mut slots, None);
        let path = file.path.clone();
        let if_match = file.if_match.clone();
        let resumable = upload_id.is_some();
        let upload_id = upload_id.map(str::to_owned);
        let (used, pending, previous) = self
            .db
            .run(move |conn| {
                let current = db::check_target(conn, &path, if_match.as_deref())?;
                Ok((
                    db::used_bytes(conn)?,
                    db::pending_bytes(conn, upload_id.as_deref())?,
                    current.map_or(0, |entry| entry.item.size_bytes),
                ))
            })
            .await?;
        let growth = file.size.saturating_sub(previous);
        if file.size > i64::MAX as u64 {
            return Err(quota_exceeded(
                self.quota_limit,
                used,
                reserved.quota,
                growth,
            ));
        }
        self.check_quota(used, pending.saturating_add(reserved.quota), growth)?;
        self.check_disk(self.disk().await?.spool_free, reserved.disk, file.size)?;
        // A resumable session already holds its growth in SQLite.
        Ok(reserve(
            &mut slots,
            if resumable { 0 } else { growth },
            file.size,
        ))
    }

    async fn reserve_part(&self, size: u64) -> ApiResult<Arc<Reservation>> {
        let mut slots = self.reservations.lock().await;
        let reserved = Reserved::collect(&mut slots, None);
        self.check_disk(self.disk().await?.spool_free, reserved.disk, size)?;
        Ok(reserve(&mut slots, 0, size))
    }

    pub(crate) async fn put(
        self: &Arc<Self>,
        file: WriteFile,
        body: ByteStream,
    ) -> ApiResult<(UploadResponse, bool)> {
        let reservation = self.reserve_file(&file, None).await?;
        let staged = self
            .spool
            .stage(body, file.size, &file.sha256, &reservation.disk_remaining)
            .await?;
        self.commit_file(file, staged, reservation, None).await
    }

    pub(crate) async fn init_upload(
        self: &Arc<Self>,
        path: DrivePath,
        size: u64,
        mime: String,
    ) -> ApiResult<InitUploadResponse> {
        let drive = self.clone();
        self.tasks
            .spawn(async move {
                let mut slots = drive.reservations.lock().await;
                let reserved = Reserved::collect(&mut slots, None);
                let free = drive.disk().await?.spool_free;
                drive.check_disk(free, reserved.disk, 0)?;
                let limit = drive.quota_limit;
                drive
                    .db
                    .run(move |conn| {
                        let tx = conn.transaction()?;
                        let current = db::check_target(&tx, &path, None)?;
                        if let Some(existing) = db::find_upload_at(&tx, &path)? {
                            if existing.size == size && existing.mime == mime {
                                return Ok(InitUploadResponse {
                                    upload_id: existing.id,
                                    part_size: existing.part_size,
                                });
                            }
                            return Err(ApiError::conflict(
                                "another resumable upload exists for this path",
                            ));
                        }
                        let used = db::used_bytes(&tx)?;
                        let promised = db::pending_bytes(&tx, None)?.saturating_add(reserved.quota);
                        let growth =
                            size.saturating_sub(current.map_or(0, |entry| entry.item.size_bytes));
                        if used
                            .checked_add(promised)
                            .and_then(|n| n.checked_add(growth))
                            .is_none_or(|total| total > limit)
                        {
                            return Err(quota_exceeded(limit, used, promised, growth));
                        }
                        let upload = Upload {
                            id: db::id("u"),
                            path,
                            size,
                            mime,
                            part_size: PART_SIZE,
                        };
                        db::insert_upload(&tx, &upload)?;
                        tx.commit()?;
                        Ok(InitUploadResponse {
                            upload_id: upload.id,
                            part_size: upload.part_size,
                        })
                    })
                    .await
            })
            .await
            .map_err(ApiError::internal)?
    }

    fn upload_lock(&self, id: &str) -> Arc<RwLock<()>> {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        self.upload_locks[hasher.finish() as usize % UPLOAD_LOCKS].clone()
    }

    pub(crate) async fn put_part(
        self: &Arc<Self>,
        id: String,
        number: u64,
        length: Option<u64>,
        hash: String,
        body: ByteStream,
    ) -> ApiResult<()> {
        let session = self.upload_lock(&id).read_owned().await;
        let upload = self
            .db
            .run(move |conn| db::require_upload(conn, &id))
            .await?;
        let size = upload.size_of_part(number)?;
        if let Some(length) = length
            && length != size
        {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ERR_CHECKSUM_MISMATCH",
                "Content-Length does not match the expected part length",
            )
            .details(json!({ "expected_size": size, "actual_size": length })));
        }
        let reservation = self.reserve_part(size).await?;
        let staged = self
            .spool
            .stage(body, size, &hash, &reservation.disk_remaining)
            .await?;
        self.publish(staged, reservation, &hash, move |conn, key, _| {
            let _session = session;
            db::record_part(conn, &upload.id, number, key, size)
        })
        .await?;
        Ok(())
    }

    pub(crate) async fn complete(
        self: &Arc<Self>,
        id: String,
        sha256: String,
        if_match: Option<String>,
    ) -> ApiResult<(UploadResponse, bool)> {
        let session = self.upload_lock(&id).write_owned().await;
        let (upload, parts) = self
            .db
            .run(move |conn| {
                let upload = db::require_upload(conn, &id)?;
                let parts = db::parts(conn, &upload)?;
                Ok((upload, parts))
            })
            .await?;
        let file = WriteFile {
            path: upload.path,
            size: upload.size,
            sha256,
            mime: upload.mime,
            if_match,
        };
        let reservation = self.reserve_file(&file, Some(&upload.id)).await?;
        let storage = self.storage.clone();
        let body = stream::iter(parts)
            .map(Ok)
            .and_then(move |part| {
                let storage = storage.clone();
                async move { storage.open(&part.key, 0..part.size).await }
            })
            .try_flatten();
        let staged = self
            .spool
            .stage(
                Box::pin(body),
                file.size,
                &file.sha256,
                &reservation.disk_remaining,
            )
            .await?;
        self.commit_file(file, staged, reservation, Some((upload.id, session)))
            .await
    }

    async fn commit_file(
        self: &Arc<Self>,
        file: WriteFile,
        staged: StagedFile,
        reservation: Arc<Reservation>,
        upload: Option<(String, tokio::sync::OwnedRwLockWriteGuard<()>)>,
    ) -> ApiResult<(UploadResponse, bool)> {
        let hash = file.sha256.clone();
        let limit = self.quota_limit;
        let ((item, created, used), disk) = self
            .publish(staged, reservation, &hash, move |conn, key, reserved| {
                let current = db::check_target(conn, &file.path, file.if_match.as_deref())?;
                let old_size = current.as_ref().map_or(0, |entry| entry.item.size_bytes);
                let used = db::used_bytes(conn)?;
                let projected = (used - old_size).saturating_add(file.size);
                if projected > limit || projected > i64::MAX as u64 {
                    return Err(quota_exceeded(
                        limit,
                        used,
                        reserved,
                        file.size.saturating_sub(old_size),
                    ));
                }
                if let Some((id, _guard)) = &upload {
                    db::remove_upload(conn, id)?;
                }
                let created = current.is_none();
                let item = db::write_file(conn, &file, key, current)?;
                let promised = db::pending_bytes(conn, None)?.saturating_add(reserved);
                if projected
                    .checked_add(promised)
                    .is_none_or(|total| total > limit)
                {
                    return Err(quota_exceeded(limit, projected, promised, 0));
                }
                Ok((item, created, projected))
            })
            .await?;
        Ok((
            UploadResponse {
                item,
                quota: self.quota_info(used, disk),
            },
            created,
        ))
    }

    async fn publish<T, F>(
        self: &Arc<Self>,
        mut staged: StagedFile,
        reservation: Arc<Reservation>,
        hash: &str,
        commit: F,
    ) -> ApiResult<(T, Disk)>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, &str, u64) -> ApiResult<T> + Send + 'static,
    {
        let drive = self.clone();
        let key = format!("{}/{}", &hash[..2], db::id("b"));
        // Once publication starts the request can go away without cancelling either
        // the metadata commit or the rollback of an unreferenced blob.
        self.tasks
            .spawn(async move {
                let mut guard = BlobRollback {
                    drive: drive.clone(),
                    key: Some(key.clone()),
                };
                let result = async {
                    drive.storage.publish(&key, staged.path()).await?;
                    staged.remove().await;
                    let mut slots = drive.reservations.lock().await;
                    let reserved = Reserved::collect(&mut slots, Some(&reservation));
                    let disk = drive.disk().await?;
                    drive.check_disk(disk.spool_free, reserved.disk, 0)?;
                    let committed = drive
                        .db
                        .run(move |conn| {
                            let tx = conn.transaction()?;
                            let value = commit(&tx, &key, reserved.quota)?;
                            tx.commit()?;
                            Ok(value)
                        })
                        .await?;
                    guard.key = None;
                    drop(reservation);
                    Ok((committed, disk))
                }
                .await;
                staged.remove().await;
                if let Some(cleanup) = guard.rollback() {
                    let _ = cleanup.await;
                }
                if result.is_ok() {
                    drive.collect_garbage().await;
                }
                result
            })
            .await
            .map_err(ApiError::internal)?
    }

    pub(crate) async fn abort(self: &Arc<Self>, id: String) -> ApiResult<()> {
        let session = self.upload_lock(&id).write_owned().await;
        let drive = self.clone();
        self.tasks
            .spawn(async move {
                {
                    let _mutex = drive.reservations.lock().await;
                    drive
                        .db
                        .run(move |conn| {
                            let tx = conn.transaction()?;
                            db::remove_upload(&tx, &id)?;
                            tx.commit()?;
                            Ok(())
                        })
                        .await?;
                }
                drop(session);
                drive.collect_garbage().await;
                Ok(())
            })
            .await
            .map_err(ApiError::internal)?
    }

    pub(crate) async fn delete(
        self: &Arc<Self>,
        path: DrivePath,
        recursive: bool,
    ) -> ApiResult<DeleteResponse> {
        let drive = self.clone();
        self.tasks
            .spawn(async move {
                let disk;
                let freed_bytes;
                let used;
                let path_string = path.to_string();
                {
                    let _mutex = drive.reservations.lock().await;
                    disk = drive.disk().await?;
                    (freed_bytes, used) = drive
                        .db
                        .run(move |conn| {
                            let tx = conn.transaction()?;
                            let freed = db::delete_tree(&tx, &path, recursive)?;
                            let used = db::used_bytes(&tx)?;
                            tx.commit()?;
                            Ok((freed, used))
                        })
                        .await?;
                }
                drive.collect_garbage().await;
                let disk = drive.disk().await.unwrap_or(disk);
                Ok(DeleteResponse {
                    deleted: true,
                    path: path_string,
                    freed_bytes,
                    quota: drive.quota_info(used, disk),
                })
            })
            .await
            .map_err(ApiError::internal)?
    }

    pub(crate) async fn open_file(
        &self,
        path: DrivePath,
        headers: &HeaderMap,
    ) -> ApiResult<(Entry, Option<std::ops::Range<u64>>, ByteStream)> {
        self.open(headers, move |conn| {
            db::require(conn, &path)?.require_file()
        })
        .await
    }

    pub(crate) async fn open_share(
        &self,
        token: String,
        headers: &HeaderMap,
    ) -> ApiResult<(Entry, Option<std::ops::Range<u64>>, ByteStream)> {
        self.open(headers, move |conn| db::shared_file(conn, &token))
            .await
    }

    async fn open(
        &self,
        headers: &HeaderMap,
        find: impl FnOnce(&Connection) -> ApiResult<Entry> + Send + 'static,
    ) -> ApiResult<(Entry, Option<std::ops::Range<u64>>, ByteStream)> {
        // Open the immutable object before allowing a metadata mutation to retire its key.
        let _mutex = self.reservations.lock().await;
        let entry = self.db.run(move |conn| find(conn)).await?;
        let size = entry.item.size_bytes;
        let range = crate::content::byte_range(headers, size)?;
        let key = entry
            .key
            .as_deref()
            .ok_or_else(|| ApiError::internal("file has no object key"))?;
        let body = self
            .storage
            .open(key, range.clone().unwrap_or(0..size))
            .await?;
        Ok((entry, range, body))
    }

    async fn collect_garbage(&self) {
        let _lock = self.garbage_lock.lock().await;
        let Ok(keys) = self.db.run(|conn| db::garbage(conn)).await else {
            return;
        };
        let mut removed = Vec::new();
        for key in keys {
            match self.storage.delete(&key).await {
                Ok(()) | Err(StorageError::NotFound) => removed.push(key),
                Err(error) => tracing::warn!(%key, %error, "could not remove retired blob"),
            }
        }
        if !removed.is_empty() {
            let _ = self
                .db
                .run(move |conn| db::forget_garbage(conn, removed))
                .await;
        }
    }
}

fn reserve(slots: &mut Vec<Weak<Reservation>>, quota: u64, disk: u64) -> Arc<Reservation> {
    let reservation = Arc::new(Reservation {
        quota,
        disk_remaining: AtomicU64::new(disk),
    });
    slots.push(Arc::downgrade(&reservation));
    reservation
}

fn quota_exceeded(limit: u64, used: u64, promised: u64, requested: u64) -> ApiError {
    ApiError::new(
        StatusCode::INSUFFICIENT_STORAGE,
        "ERR_QUOTA_EXCEEDED",
        "storage quota exceeded",
    )
    .details(json!({
        "requested_bytes": requested, "used_bytes": used, "limit_bytes": limit,
        "remaining_bytes": limit.saturating_sub(used).saturating_sub(promised),
    }))
}

struct BlobRollback {
    drive: Arc<Drive>,
    key: Option<String>,
}

impl BlobRollback {
    fn rollback(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let key = self.key.take()?;
        let drive = self.drive.clone();
        Some(self.drive.tasks.spawn(async move {
            if let Err(error) = drive.storage.delete(&key).await
                && !matches!(error, StorageError::NotFound)
            {
                tracing::warn!(%key, %error, "blob rollback will retry on the next cleanup");
                let _ = drive
                    .db
                    .run(move |conn| db::enqueue_garbage(conn, &key))
                    .await;
            }
        }))
    }
}

impl Drop for BlobRollback {
    fn drop(&mut self) {
        self.rollback();
    }
}

impl From<StorageError> for ApiError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound => Self::not_found("file content is unavailable"),
            StorageError::BodyInterrupted => Self::bad("upload body interrupted"),
            StorageError::Mismatch {
                expected_size,
                actual_size,
                expected_sha256,
                actual_sha256,
            } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ERR_CHECKSUM_MISMATCH",
                "upload discarded: content does not match X-Content-SHA256 / expected size",
            )
            .details(
                json!({ "expected_size": expected_size, "actual_size": actual_size,
                    "expected_sha256": expected_sha256, "actual_sha256": actual_sha256 }),
            ),
            other => Self::internal(other),
        }
    }
}

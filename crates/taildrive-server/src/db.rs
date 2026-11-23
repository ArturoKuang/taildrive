use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::json;
use taildrive_core::{
    models::{FileItem, FileType, ShareItem},
    path::{DrivePath, parent_path, parse_path},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::task::spawn_blocking;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    parent_path TEXT NOT NULL,
    name TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('file', 'folder')),
    object_key TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    mime TEXT,
    sha256 TEXT,
    version INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS files_parent_path ON files (parent_path, path);

CREATE TABLE IF NOT EXISTS usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    used_bytes INTEGER NOT NULL
);
INSERT OR IGNORE INTO usage SELECT 1, COALESCE(SUM(size_bytes), 0) FROM files;
CREATE TRIGGER IF NOT EXISTS files_insert_usage AFTER INSERT ON files BEGIN
    UPDATE usage SET used_bytes = used_bytes + NEW.size_bytes WHERE singleton = 1;
END;
CREATE TRIGGER IF NOT EXISTS files_update_usage AFTER UPDATE OF size_bytes ON files BEGIN
    UPDATE usage SET used_bytes = used_bytes - OLD.size_bytes + NEW.size_bytes WHERE singleton = 1;
END;
CREATE TRIGGER IF NOT EXISTS files_delete_usage AFTER DELETE ON files BEGIN
    UPDATE usage SET used_bytes = used_bytes - OLD.size_bytes WHERE singleton = 1;
END;

CREATE TABLE IF NOT EXISTS uploads (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL,
    mime TEXT NOT NULL,
    part_size INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS upload_parts (
    upload_id TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
    part_num INTEGER NOT NULL,
    object_key TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    PRIMARY KEY (upload_id, part_num)
);

CREATE TABLE IF NOT EXISTS shares (
    id TEXT PRIMARY KEY,
    file_id TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    token TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS shares_file ON shares (file_id);

CREATE TABLE IF NOT EXISTS garbage (object_key TEXT PRIMARY KEY);
";

#[derive(Clone)]
pub(crate) struct Db(Arc<Mutex<Connection>>);

pub(crate) type StartupResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

impl Db {
    pub(crate) async fn open(path: PathBuf) -> StartupResult<Self> {
        let connection = spawn_blocking(move || -> rusqlite::Result<Connection> {
            let connection = Connection::open(path)?;
            connection.pragma_update(None, "journal_mode", "WAL")?;
            connection.pragma_update(None, "synchronous", "FULL")?;
            connection.pragma_update(None, "foreign_keys", true)?;
            connection.execute_batch(SCHEMA)?;
            Ok(connection)
        })
        .await??;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }

    pub(crate) async fn run<T, F>(&self, operation: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> ApiResult<T> + Send + 'static,
    {
        let connection = self.0.clone();
        spawn_blocking(move || {
            let mut conn = connection
                .lock()
                .map_err(|_| ApiError::internal("SQLite lock poisoned"))?;
            operation(&mut conn)
        })
        .await
        .map_err(ApiError::internal)?
    }
}

pub(crate) struct Entry {
    pub item: FileItem,
    pub key: Option<String>,
}

impl Entry {
    pub(crate) fn require_file(self) -> ApiResult<Self> {
        if self.item.kind == FileType::File {
            Ok(self)
        } else {
            Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "ERR_NOT_A_FILE",
                "path is a folder",
            )
            .details(json!({ "path": self.item.path })))
        }
    }
}

fn entry(row: &Row<'_>) -> rusqlite::Result<Entry> {
    let kind = match row.get_ref("type")?.as_str()? {
        "file" => FileType::File,
        _ => FileType::Folder,
    };
    let sha256: Option<String> = row.get("sha256")?;
    Ok(Entry {
        item: FileItem {
            id: row.get("id")?,
            name: row.get("name")?,
            path: row.get("path")?,
            kind,
            size_bytes: row.get("size_bytes")?,
            mime: row.get("mime")?,
            etag: sha256.as_ref().map(|hash| format!("\"{hash}\"")),
            sha256,
            version: row.get("version")?,
            updated_at: row.get("updated_at")?,
        },
        key: row.get("object_key")?,
    })
}

pub(crate) fn get(conn: &Connection, path: &DrivePath) -> ApiResult<Option<Entry>> {
    Ok(conn
        .query_row(
            "SELECT * FROM files WHERE path = ?1",
            [path.as_str()],
            entry,
        )
        .optional()?)
}

pub(crate) fn require(conn: &Connection, path: &DrivePath) -> ApiResult<Entry> {
    get(conn, path)?.ok_or_else(|| {
        ApiError::not_found("no such file or folder").details(json!({ "path": path.as_str() }))
    })
}

fn check_ancestors(conn: &Connection, path: &DrivePath) -> ApiResult<()> {
    for ancestor in path.ancestors() {
        if get(conn, &ancestor)?.is_some_and(|entry| entry.item.kind == FileType::File) {
            return Err(ApiError::conflict("a parent path is a file")
                .details(json!({ "path": path.as_str(), "file": ancestor.as_str() })));
        }
    }
    Ok(())
}

pub(crate) fn check_target(
    conn: &Connection,
    path: &DrivePath,
    if_match: Option<&str>,
) -> ApiResult<Option<Entry>> {
    let current = get(conn, path)?;
    if current
        .as_ref()
        .is_some_and(|entry| entry.item.kind == FileType::Folder)
    {
        return Err(
            ApiError::conflict("path is a folder").details(json!({ "path": path.as_str() }))
        );
    }
    check_ancestors(conn, path)?;
    if let Some(condition) = if_match {
        let matches = current.as_ref().is_some_and(|entry| {
            condition.trim() == "*"
                || condition.split(',').map(str::trim).any(|tag| {
                    Some(tag) == entry.item.etag.as_deref()
                        || Some(tag) == entry.item.sha256.as_deref()
                })
        });
        if !matches {
            return Err(
                ApiError::conflict("If-Match does not match the current version").details(json!({
                    "path": path.as_str(),
                    "current_etag": current.and_then(|entry| entry.item.etag),
                })),
            );
        }
    }
    Ok(current)
}

pub(crate) fn list(
    conn: &Connection,
    path: &DrivePath,
    after: String,
    limit: usize,
) -> ApiResult<Vec<FileItem>> {
    if !path.is_root() && require(conn, path)?.item.kind != FileType::Folder {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "ERR_NOT_A_FOLDER",
            "path is a file",
        )
        .details(json!({ "path": path.as_str() })));
    }
    let mut stmt = conn.prepare(
        "SELECT * FROM files WHERE parent_path = ?1 AND path > ?2 ORDER BY path LIMIT ?3",
    )?;
    let entries = stmt.query_map(params![path.as_str(), after, limit], |row| {
        Ok(entry(row)?.item)
    })?;
    Ok(entries.collect::<Result<_, _>>()?)
}

pub(crate) fn create_folder(conn: &Connection, path: &DrivePath) -> ApiResult<FileItem> {
    if get(conn, path)?.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "ERR_ALREADY_EXISTS",
            "path already exists",
        )
        .details(json!({ "path": path.as_str() })));
    }
    check_ancestors(conn, path)?;
    insert_folders(conn, path.ancestors().iter().chain(std::iter::once(path)))?;
    Ok(require(conn, path)?.item)
}

fn insert_folders<'a>(
    conn: &Connection,
    paths: impl Iterator<Item = &'a DrivePath>,
) -> ApiResult<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO files (id, path, parent_path, name, type, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'folder', ?5) ON CONFLICT(path) DO NOTHING",
    )?;
    let now = timestamp(OffsetDateTime::now_utc())?;
    for path in paths {
        stmt.execute(params![
            id("d"),
            path.as_str(),
            parent_path(path.as_str()),
            path.name(),
            now
        ])?;
    }
    Ok(())
}

pub(crate) struct WriteFile {
    pub path: DrivePath,
    pub size: u64,
    pub sha256: String,
    pub mime: String,
    pub if_match: Option<String>,
}

pub(crate) fn write_file(
    conn: &Connection,
    file: &WriteFile,
    key: &str,
    current: Option<Entry>,
) -> ApiResult<FileItem> {
    let WriteFile {
        path,
        size,
        sha256,
        mime,
        ..
    } = file;
    insert_folders(conn, path.ancestors().iter())?;
    let now = timestamp(OffsetDateTime::now_utc())?;
    if let Some(current) = current {
        conn.execute(
            "UPDATE files SET object_key = ?1, size_bytes = ?2, mime = ?3, sha256 = ?4,
             updated_at = ?5, version = version + 1 WHERE id = ?6",
            params![key, size, mime, sha256, now, current.item.id],
        )?;
        if let Some(old) = current.key {
            enqueue_garbage(conn, &old)?;
        }
    } else {
        conn.execute(
            "INSERT INTO files (id, path, parent_path, name, type, object_key, size_bytes, mime, sha256, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'file', ?5, ?6, ?7, ?8, ?9)",
            params![id("f"), path.as_str(), parent_path(path.as_str()), path.name(), key, size, mime, sha256, now],
        )?;
    }
    Ok(require(conn, path)?.item)
}

pub(crate) fn delete_tree(conn: &Connection, path: &DrivePath, recursive: bool) -> ApiResult<u64> {
    let entry = require(conn, path)?;
    if !recursive
        && entry.item.kind == FileType::Folder
        && conn
            .query_row(
                "SELECT 1 FROM files WHERE parent_path = ?1 LIMIT 1",
                [path.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "ERR_NOT_EMPTY",
            "folder is not empty; pass recursive=true",
        )
        .details(json!({ "path": path.as_str() })));
    }

    // '/' and '0' are adjacent in SQLite's binary sort order. Every descendant
    // falls in this half-open interval, even with Unicode or wildcard characters.
    let bounds = params![path.as_str(), format!("{path}/"), format!("{path}0")];
    let freed = conn.query_row(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM files WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        bounds, |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO garbage SELECT object_key FROM files
         WHERE object_key IS NOT NULL AND (path = ?1 OR (path >= ?2 AND path < ?3))",
        bounds,
    )?;
    conn.execute(
        "DELETE FROM files WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        bounds,
    )?;
    Ok(freed)
}

pub(crate) fn used_bytes(conn: &Connection) -> ApiResult<u64> {
    Ok(conn.query_row(
        "SELECT used_bytes FROM usage WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?)
}

pub(crate) fn pending_bytes(conn: &Connection, exclude: Option<&str>) -> ApiResult<u64> {
    let mut stmt = conn.prepare(
        "SELECT MAX(uploads.size_bytes - COALESCE(files.size_bytes, 0), 0)
         FROM uploads LEFT JOIN files ON uploads.path = files.path WHERE (?1 IS NULL OR uploads.id <> ?1)",
    )?;
    let mut values = stmt.query_map([exclude], |row| row.get::<_, u64>(0))?;
    values.try_fold(0_u64, |sum, size| Ok(sum.saturating_add(size?)))
}

#[derive(Debug)]
pub(crate) struct Upload {
    pub id: String,
    pub path: DrivePath,
    pub size: u64,
    pub mime: String,
    pub part_size: u64,
}

impl Upload {
    pub(crate) fn part_count(&self) -> u64 {
        self.size.div_ceil(self.part_size)
    }

    pub(crate) fn size_of_part(&self, number: u64) -> ApiResult<u64> {
        if number == 0 || number > self.part_count() {
            return Err(ApiError::bad("part number is outside the upload"));
        }
        Ok(self
            .part_size
            .min(self.size - (number - 1) * self.part_size))
    }
}

pub(crate) struct Part {
    pub key: String,
    pub size: u64,
}

fn find_upload(conn: &Connection, column: &str, value: &str) -> ApiResult<Option<Upload>> {
    // Only the two static column names in the callers below reach this query.
    let raw = conn
        .query_row(
            &format!(
                "SELECT id, path, size_bytes, mime, part_size FROM uploads WHERE {column} = ?1"
            ),
            [value],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, String>(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(id, path, size, mime, part_size)| {
        Ok(Upload {
            id,
            path: parse_path(&path).map_err(ApiError::internal)?,
            size,
            mime,
            part_size,
        })
    })
    .transpose()
}

pub(crate) fn find_upload_at(conn: &Connection, path: &DrivePath) -> ApiResult<Option<Upload>> {
    find_upload(conn, "path", path.as_str())
}

pub(crate) fn require_upload(conn: &Connection, id: &str) -> ApiResult<Upload> {
    find_upload(conn, "id", id)?.ok_or_else(|| ApiError::not_found("no such upload"))
}

pub(crate) fn insert_upload(conn: &Connection, upload: &Upload) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO uploads (id, path, size_bytes, mime, part_size) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            upload.id,
            upload.path.as_str(),
            upload.size,
            upload.mime,
            upload.part_size
        ],
    )?;
    Ok(())
}

pub(crate) fn parts(conn: &Connection, upload: &Upload) -> ApiResult<Vec<Part>> {
    let mut stmt = conn.prepare(
        "SELECT object_key, size_bytes FROM upload_parts WHERE upload_id = ?1 ORDER BY part_num",
    )?;
    let parts = stmt.query_map([&upload.id], |row| {
        Ok(Part {
            key: row.get(0)?,
            size: row.get(1)?,
        })
    })?;
    let parts: Vec<_> = parts.collect::<Result<_, _>>()?;
    if parts.len() as u64 != upload.part_count() {
        return Err(
            ApiError::conflict("resumable upload is missing parts").details(json!({
                "expected_parts": upload.part_count(), "received_parts": parts.len(),
            })),
        );
    }
    Ok(parts)
}

pub(crate) fn record_part(
    conn: &Connection,
    id: &str,
    number: u64,
    key: &str,
    size: u64,
) -> ApiResult<()> {
    require_upload(conn, id)?;
    conn.execute(
        "INSERT INTO garbage SELECT object_key FROM upload_parts WHERE upload_id = ?1 AND part_num = ?2",
        params![id, number],
    )?;
    conn.execute(
        "INSERT INTO upload_parts (upload_id, part_num, object_key, size_bytes) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(upload_id, part_num) DO UPDATE SET object_key = excluded.object_key, size_bytes = excluded.size_bytes",
        params![id, number, key, size],
    )?;
    Ok(())
}

pub(crate) fn remove_upload(conn: &Connection, id: &str) -> ApiResult<()> {
    require_upload(conn, id)?;
    conn.execute(
        "INSERT INTO garbage SELECT object_key FROM upload_parts WHERE upload_id = ?1",
        [id],
    )?;
    conn.execute("DELETE FROM uploads WHERE id = ?1", [id])?;
    Ok(())
}

pub(crate) fn enqueue_garbage(conn: &Connection, key: &str) -> ApiResult<()> {
    conn.execute("INSERT OR IGNORE INTO garbage VALUES (?1)", [key])?;
    Ok(())
}

pub(crate) fn create_share(
    conn: &Connection,
    path: &DrivePath,
    token: String,
    expires_in: u64,
) -> ApiResult<ShareItem> {
    let file = require(conn, path)?.require_file()?;
    let now = OffsetDateTime::now_utc();
    let expires = i64::try_from(expires_in)
        .ok()
        .and_then(|seconds| now.checked_add(time::Duration::seconds(seconds)))
        .filter(|date| expires_in > 0 && date.year() <= 9999)
        .ok_or_else(|| {
            ApiError::bad("expires_in_seconds must be positive and within the supported date range")
        })?;
    let id = id("s");
    conn.execute(
        "INSERT INTO shares (id, file_id, token, expires_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, file.item.id, token, share_time(expires), share_time(now)],
    )?;
    Ok(ShareItem {
        id,
        path: file.item.path,
        url: format!("/s/{token}"),
        created_at: share_timestamp(now)?,
        expires_at: share_timestamp(expires)?,
    })
}

pub(crate) fn list_shares(conn: &Connection) -> ApiResult<Vec<ShareItem>> {
    let mut stmt = conn.prepare(
        "SELECT shares.id, files.path, shares.token, shares.expires_at, shares.created_at
         FROM shares JOIN files ON shares.file_id = files.id
         WHERE shares.expires_at > ?1 ORDER BY shares.created_at DESC, shares.id DESC",
    )?;
    let rows = stmt.query_map([share_time(OffsetDateTime::now_utc())], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (id, path, token, expires, created) = row?;
        Ok(ShareItem {
            id,
            path,
            url: format!("/s/{token}"),
            expires_at: share_timestamp(
                OffsetDateTime::from_unix_timestamp_nanos(i128::from(expires) * 1_000_000)
                    .map_err(ApiError::internal)?,
            )?,
            created_at: share_timestamp(
                OffsetDateTime::from_unix_timestamp_nanos(i128::from(created) * 1_000_000)
                    .map_err(ApiError::internal)?,
            )?,
        })
    })
    .collect()
}

pub(crate) fn revoke_share(conn: &Connection, id: &str) -> ApiResult<()> {
    if conn.execute("DELETE FROM shares WHERE id = ?1", [id])? == 0 {
        return Err(ApiError::not_found("no such share"));
    }
    Ok(())
}

pub(crate) fn shared_file(conn: &Connection, token: &str) -> ApiResult<Entry> {
    conn.query_row(
        "SELECT files.* FROM files JOIN shares ON shares.file_id = files.id
         WHERE shares.token = ?1 AND shares.expires_at > ?2",
        params![token, share_time(OffsetDateTime::now_utc())],
        entry,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found("share is unavailable"))
}

pub(crate) fn id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::now_v7().simple())
}

fn timestamp(time: OffsetDateTime) -> ApiResult<String> {
    time.replace_nanosecond(0)
        .map_err(ApiError::internal)?
        .format(&Rfc3339)
        .map_err(ApiError::internal)
}

pub(crate) fn garbage(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT object_key FROM garbage")?;
    let keys = stmt.query_map([], |row| row.get(0))?;
    Ok(keys.collect::<Result<_, _>>()?)
}

pub(crate) fn forget_garbage(conn: &mut Connection, keys: Vec<String>) -> ApiResult<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare("DELETE FROM garbage WHERE object_key = ?1")?;
        for key in keys {
            stmt.execute([key])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn share_time(time: OffsetDateTime) -> i64 {
    (time.unix_timestamp_nanos() / 1_000_000) as i64
}

fn share_timestamp(time: OffsetDateTime) -> ApiResult<String> {
    time.replace_nanosecond(time.millisecond() as u32 * 1_000_000)
        .map_err(ApiError::internal)?
        .format(&Rfc3339)
        .map_err(ApiError::internal)
}

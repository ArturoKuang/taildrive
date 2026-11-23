"""SQLite metadata: the drive's folder tree and the object key behind each file.

Every call runs on the event loop thread with no awaits inside, so a
`transaction()` block is atomic with respect to other requests.
"""

import sqlite3
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Literal

from taildrive.paths import ancestors_of, name_of, parent_of

EntryType = Literal["file", "folder"]

_SCHEMA = """
CREATE TABLE IF NOT EXISTS files (
    id          TEXT PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    parent_path TEXT NOT NULL,
    name        TEXT NOT NULL,
    type        TEXT NOT NULL CHECK (type IN ('file', 'folder')),
    object_key  TEXT,
    size_bytes  INTEGER NOT NULL DEFAULT 0,
    mime        TEXT,
    sha256      TEXT,
    version     INTEGER NOT NULL DEFAULT 1,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS files_parent_path ON files (parent_path, path);
"""


@dataclass(frozen=True)
class Entry:
    id: str
    path: str
    parent_path: str
    name: str
    type: EntryType
    object_key: str | None
    size_bytes: int
    mime: str | None
    sha256: str | None
    version: int
    updated_at: str

    @property
    def etag(self) -> str | None:
        return f'"{self.sha256}"' if self.sha256 else None


class MetadataStore:
    def __init__(self, db_path: Path) -> None:
        db_path.parent.mkdir(parents=True, exist_ok=True)
        self._db = sqlite3.connect(db_path, isolation_level=None, check_same_thread=False)
        self._db.row_factory = sqlite3.Row
        self._db.execute("PRAGMA journal_mode=WAL")
        self._db.execute("PRAGMA synchronous=FULL")
        self._db.executescript(_SCHEMA)

    @contextmanager
    def transaction(self) -> Iterator[None]:
        self._db.execute("BEGIN IMMEDIATE")
        try:
            yield
        except BaseException:
            self._db.execute("ROLLBACK")
            raise
        self._db.execute("COMMIT")

    def get(self, path: str) -> Entry | None:
        row = self._db.execute("SELECT * FROM files WHERE path = ?", (path,)).fetchone()
        return _entry(row) if row else None

    def list_children(self, parent: str, *, after: str | None, limit: int) -> list[Entry]:
        rows = self._db.execute(
            "SELECT * FROM files WHERE parent_path = ? AND path > ? ORDER BY path LIMIT ?",
            (parent, after or "", limit),
        ).fetchall()
        return [_entry(r) for r in rows]

    def has_children(self, path: str) -> bool:
        return self._db.execute("SELECT 1 FROM files WHERE parent_path = ? LIMIT 1", (path,)).fetchone() is not None

    def used_bytes(self) -> int:
        return self._db.execute("SELECT COALESCE(SUM(size_bytes), 0) FROM files WHERE type = 'file'").fetchone()[0]

    def blocking_ancestor(self, path: str) -> str | None:
        """The first ancestor of `path` that exists as a file, if any."""
        for ancestor in ancestors_of(path):
            entry = self.get(ancestor)
            if entry and entry.type == "file":
                return ancestor
        return None

    def create_folders(self, paths: list[str]) -> None:
        now = _now()
        self._db.executemany(
            "INSERT OR IGNORE INTO files (id, path, parent_path, name, type, updated_at)"
            " VALUES (?, ?, ?, ?, 'folder', ?)",
            [(_new_id("d"), p, parent_of(p), name_of(p), now) for p in paths],
        )

    def write_file(
        self, path: str, *, object_key: str, size_bytes: int, sha256: str, mime: str, existing: Entry | None
    ) -> Entry:
        self.create_folders(ancestors_of(path))
        now = _now()
        if existing:
            self._db.execute(
                "UPDATE files SET object_key = ?, size_bytes = ?, sha256 = ?, mime = ?,"
                " version = version + 1, updated_at = ? WHERE id = ?",
                (object_key, size_bytes, sha256, mime, now, existing.id),
            )
        else:
            self._db.execute(
                "INSERT INTO files (id, path, parent_path, name, type, object_key, size_bytes, mime, sha256,"
                " version, updated_at) VALUES (?, ?, ?, ?, 'file', ?, ?, ?, ?, 1, ?)",
                (_new_id("f"), path, parent_of(path), name_of(path), object_key, size_bytes, mime, sha256, now),
            )
        entry = self.get(path)
        assert entry is not None
        return entry

    def delete_tree(self, path: str) -> list[Entry]:
        """Delete `path` and every descendant; return the removed entries."""
        prefix = path + "/"
        # substr instead of LIKE so `%` and `_` in names are not wildcards.
        where = "path = ? OR substr(path, 1, ?) = ?"
        args = (path, len(prefix), prefix)
        removed = [_entry(r) for r in self._db.execute(f"SELECT * FROM files WHERE {where}", args)]
        self._db.execute(f"DELETE FROM files WHERE {where}", args)
        return removed


def _entry(row: sqlite3.Row) -> Entry:
    return Entry(**dict(row))


def _new_id(prefix: str) -> str:
    return f"{prefix}_{uuid.uuid4().hex}"


def _now() -> str:
    return datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")

"""Drive operations: path rules, If-Match, quota + disk floor, and the upload commit protocol."""

import uuid
from collections.abc import AsyncIterable
from dataclasses import dataclass
from typing import Literal

from taildrive.errors import ApiError
from taildrive.metadata import Entry, MetadataStore
from taildrive.paths import ROOT, ancestors_of
from taildrive.storage import BlobMismatch, StorageAdapter

QuotaStatus = Literal["ok", "warn", "full"]


@dataclass(frozen=True)
class Quota:
    used_bytes: int
    limit_bytes: int
    remaining_bytes: int
    disk_free_bytes: int
    disk_floor_bytes: int
    status: QuotaStatus


@dataclass(frozen=True)
class Deleted:
    path: str
    freed_bytes: int


class Drive:
    def __init__(self, store: MetadataStore, storage: StorageAdapter, *, quota_bytes: int, free_floor_bytes: int):
        self.store = store
        self.storage = storage
        self.quota_bytes = quota_bytes
        self.free_floor_bytes = free_floor_bytes
        # Bytes promised to in-flight uploads; mutated only on the event loop thread.
        self._reserved_quota = 0
        self._reserved_disk = 0

    async def quota(self) -> Quota:
        used = self.store.used_bytes()
        free = await self.storage.disk_free_bytes()
        ratio = used / self.quota_bytes if self.quota_bytes else 1.0
        status: QuotaStatus
        if ratio >= 0.95 or free < self.free_floor_bytes:
            status = "full"
        elif ratio >= 0.80:
            status = "warn"
        else:
            status = "ok"
        return Quota(
            used_bytes=used,
            limit_bytes=self.quota_bytes,
            remaining_bytes=max(self.quota_bytes - used, 0),
            disk_free_bytes=free,
            disk_floor_bytes=self.free_floor_bytes,
            status=status,
        )

    def list_folder(self, path: str, *, after: str | None, limit: int) -> tuple[list[Entry], str | None]:
        if path != ROOT:
            entry = self._require(path)
            if entry.type != "folder":
                raise ApiError(400, "ERR_NOT_A_FOLDER", "path is a file", details={"path": path})
        page = self.store.list_children(path, after=after, limit=limit + 1)
        if len(page) > limit:
            return page[:limit], page[limit - 1].path
        return page, None

    def create_folder(self, path: str) -> Entry:
        with self.store.transaction():
            if self.store.get(path):
                raise ApiError(409, "ERR_ALREADY_EXISTS", "path already exists", details={"path": path})
            self._check_ancestors(path)
            self.store.create_folders([*ancestors_of(path), path])
            entry = self.store.get(path)
        assert entry is not None
        return entry

    def read_file(self, path: str) -> Entry:
        entry = self._require(path)
        if entry.type != "file":
            raise ApiError(400, "ERR_NOT_A_FILE", "path is a folder", details={"path": path})
        return entry

    async def put_file(
        self,
        path: str,
        chunks: AsyncIterable[bytes],
        *,
        size: int,
        sha256: str,
        mime: str,
        if_match: str | None,
    ) -> tuple[Entry, bool]:
        """Store a file; returns (entry, created). Checks run before streaming and again before commit."""
        existing = self._check_put_target(path, if_match)
        quota_delta = max(size - (existing.size_bytes if existing else 0), 0)
        await self._check_capacity(quota_delta, size)

        self._reserved_quota += quota_delta
        self._reserved_disk += size
        try:
            key = f"{sha256[:2]}/{uuid.uuid4().hex}"
            try:
                await self.storage.put(key, chunks, size, sha256)
            except BlobMismatch as e:
                raise ApiError(
                    422,
                    "ERR_CHECKSUM_MISMATCH",
                    "upload discarded: content does not match X-Content-SHA256 / Content-Length",
                    details={
                        "expected_sha256": e.expected_sha256,
                        "actual_sha256": e.actual_sha256,
                        "expected_size": e.expected_size,
                        "actual_size": e.actual_size,
                    },
                ) from e
            try:
                with self.store.transaction():
                    # Another request may have changed the file while we streamed.
                    current = self._check_put_target(path, if_match)
                    delta = size - (current.size_bytes if current else 0)
                    others_reserved = self._reserved_quota - quota_delta
                    if self.store.used_bytes() + others_reserved + delta > self.quota_bytes:
                        raise self._quota_exceeded(max(delta, 0))
                    entry = self.store.write_file(
                        path, object_key=key, size_bytes=size, sha256=sha256, mime=mime, existing=current
                    )
            except BaseException:
                await self.storage.delete(key)
                raise
        finally:
            self._reserved_quota -= quota_delta
            self._reserved_disk -= size

        if current and current.object_key:
            await self.storage.delete(current.object_key)
        return entry, current is None

    async def delete(self, path: str, *, recursive: bool) -> Deleted:
        with self.store.transaction():
            entry = self._require(path)
            if entry.type == "folder" and not recursive and self.store.has_children(path):
                raise ApiError(
                    409, "ERR_NOT_EMPTY", "folder is not empty; pass recursive=true", details={"path": path}
                )
            removed = self.store.delete_tree(path)
        # Metadata first: a crash here leaves orphan blobs, never dangling entries.
        for item in removed:
            if item.object_key:
                await self.storage.delete(item.object_key)
        return Deleted(path=path, freed_bytes=sum(e.size_bytes for e in removed if e.type == "file"))

    def _require(self, path: str) -> Entry:
        entry = self.store.get(path)
        if entry is None:
            raise ApiError(404, "ERR_NOT_FOUND", "no such file or folder", details={"path": path})
        return entry

    def _check_ancestors(self, path: str) -> None:
        blocker = self.store.blocking_ancestor(path)
        if blocker is not None:
            raise ApiError(409, "ERR_CONFLICT", "a parent path is a file", details={"path": path, "file": blocker})

    def _check_put_target(self, path: str, if_match: str | None) -> Entry | None:
        existing = self.store.get(path)
        if existing and existing.type == "folder":
            raise ApiError(409, "ERR_CONFLICT", "path is a folder", details={"path": path})
        self._check_ancestors(path)
        if if_match is not None and not _etag_matches(if_match, existing):
            raise ApiError(
                409,
                "ERR_CONFLICT",
                "If-Match does not match the current version",
                details={"path": path, "current_etag": existing.etag if existing else None},
            )
        return existing

    async def _check_capacity(self, quota_delta: int, disk_bytes: int) -> None:
        if self.store.used_bytes() + self._reserved_quota + quota_delta > self.quota_bytes:
            raise self._quota_exceeded(quota_delta)
        free = await self.storage.disk_free_bytes()
        if free - self._reserved_disk - disk_bytes < self.free_floor_bytes:
            raise ApiError(
                507,
                "ERR_DISK_FLOOR",
                "upload would push the disk below its free-space floor",
                details={
                    "requested_bytes": disk_bytes,
                    "disk_free_bytes": free,
                    "disk_floor_bytes": self.free_floor_bytes,
                },
            )

    def _quota_exceeded(self, requested: int) -> ApiError:
        used = self.store.used_bytes()
        return ApiError(
            507,
            "ERR_QUOTA_EXCEEDED",
            "storage quota exceeded",
            details={
                "requested_bytes": requested,
                "used_bytes": used,
                "limit_bytes": self.quota_bytes,
                "remaining_bytes": max(self.quota_bytes - used - self._reserved_quota, 0),
            },
        )


def _etag_matches(if_match: str, existing: Entry | None) -> bool:
    if existing is None:
        return False
    if if_match.strip() == "*":
        return True
    candidates = {tag.strip() for tag in if_match.split(",")}
    return existing.etag in candidates or existing.sha256 in candidates

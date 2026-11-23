"""Blobs on a local volume: `<data_dir>/blobs/<key>`, staged in `<data_dir>/tmp/`.

Staging and blobs share one filesystem so commit is a single atomic rename.
"""

import hashlib
import hmac
import os
import re
import shutil
import uuid
from collections.abc import AsyncIterable, AsyncIterator
from pathlib import Path

from taildrive.storage.base import BlobMismatch, StoredBlob

_KEY = re.compile(r"[0-9a-f]{2}/[0-9A-Za-z_-]{1,128}")
_CHUNK = 1024 * 1024


class LocalFsStorageAdapter:
    driver_name = "local-fs"

    def __init__(self, data_dir: Path) -> None:
        self.root = data_dir
        self.blobs_dir = data_dir / "blobs"
        self.tmp_dir = data_dir / "tmp"
        self.blobs_dir.mkdir(parents=True, exist_ok=True)
        self.tmp_dir.mkdir(parents=True, exist_ok=True)
        # Anything still staged is from a crashed upload that was never committed.
        for leftover in self.tmp_dir.iterdir():
            leftover.unlink(missing_ok=True)

    async def put(
        self, key: str, chunks: AsyncIterable[bytes], expected_size: int, expected_sha256: str
    ) -> StoredBlob:
        dest = self._blob_path(key)
        staged = self.tmp_dir / uuid.uuid4().hex
        hasher = hashlib.sha256()
        size = 0
        try:
            with open(staged, "xb") as f:
                async for chunk in chunks:
                    size += len(chunk)
                    if size > expected_size:
                        raise BlobMismatch(
                            expected_size=expected_size,
                            actual_size=size,
                            expected_sha256=expected_sha256,
                            actual_sha256=None,
                        )
                    hasher.update(chunk)
                    f.write(chunk)
                f.flush()
                os.fsync(f.fileno())
            actual = hasher.hexdigest()
            if size != expected_size or not hmac.compare_digest(actual, expected_sha256):
                raise BlobMismatch(
                    expected_size=expected_size,
                    actual_size=size,
                    expected_sha256=expected_sha256,
                    actual_sha256=actual,
                )
            dest.parent.mkdir(exist_ok=True)
            os.replace(staged, dest)
        except BaseException:
            # BaseException: a client disconnect cancels the task mid-stream.
            staged.unlink(missing_ok=True)
            raise
        return StoredBlob(key=key, size_bytes=size, sha256=actual)

    async def get(self, key: str, start: int = 0, end: int | None = None) -> AsyncIterator[bytes]:
        remaining = None if end is None else end - start + 1
        with open(self._blob_path(key), "rb") as f:
            f.seek(start)
            while remaining is None or remaining > 0:
                data = f.read(_CHUNK if remaining is None else min(_CHUNK, remaining))
                if not data:
                    return
                if remaining is not None:
                    remaining -= len(data)
                yield data

    async def delete(self, key: str) -> int:
        path = self._blob_path(key)
        try:
            size = path.stat().st_size
            path.unlink()
        except FileNotFoundError:
            return 0
        return size

    async def disk_free_bytes(self) -> int:
        return shutil.disk_usage(self.root).free

    def _blob_path(self, key: str) -> Path:
        if not _KEY.fullmatch(key):
            raise ValueError(f"invalid object key: {key!r}")
        return self.blobs_dir / key

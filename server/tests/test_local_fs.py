import asyncio
import hashlib
from collections.abc import AsyncIterator
from pathlib import Path

import pytest

from taildrive.config import S3Settings, Settings
from taildrive.storage import BlobMismatch, LocalFsStorageAdapter, S3StorageAdapter, StorageAdapter, build_storage

KEY = "ab/blob1"


async def chunks(*parts: bytes) -> AsyncIterator[bytes]:
    for part in parts:
        yield part


async def read_all(adapter: LocalFsStorageAdapter, key: str, start: int = 0, end: int | None = None) -> bytes:
    return b"".join([c async for c in adapter.get(key, start, end)])


def put(adapter: LocalFsStorageAdapter, data_parts: tuple[bytes, ...], size: int, digest: str):
    return asyncio.run(adapter.put(KEY, chunks(*data_parts), size, digest))


@pytest.fixture
def adapter(tmp_path: Path) -> LocalFsStorageAdapter:
    return LocalFsStorageAdapter(tmp_path)


def test_put_commits_verified_blob_and_leaves_no_staging(adapter: LocalFsStorageAdapter):
    data = b"hello " * 1000
    blob = put(adapter, (data[:10], data[10:]), len(data), hashlib.sha256(data).hexdigest())

    assert blob.size_bytes == len(data)
    assert asyncio.run(read_all(adapter, KEY)) == data
    assert list(adapter.tmp_dir.iterdir()) == []


@pytest.mark.parametrize(
    ("parts", "declared_size"),
    [
        ((b"hello",), 5),  # right size, wrong hash
        ((b"hel",), 5),  # stream shorter than declared
        ((b"hello", b"-extra"), 5),  # stream longer than declared, aborted mid-stream
    ],
)
def test_mismatch_discards_staged_bytes_and_commits_nothing(adapter: LocalFsStorageAdapter, parts, declared_size):
    with pytest.raises(BlobMismatch):
        put(adapter, parts, declared_size, hashlib.sha256(b"other").hexdigest())

    assert list(adapter.tmp_dir.iterdir()) == []
    assert not (adapter.blobs_dir / KEY).exists()


def test_failure_mid_stream_discards_staged_bytes(adapter: LocalFsStorageAdapter):
    async def broken() -> AsyncIterator[bytes]:
        yield b"partial"
        raise ConnectionResetError("client went away")

    with pytest.raises(ConnectionResetError):
        asyncio.run(adapter.put(KEY, broken(), 100, "0" * 64))

    assert list(adapter.tmp_dir.iterdir()) == []


def test_get_streams_inclusive_byte_ranges(adapter: LocalFsStorageAdapter):
    data = b"0123456789"
    put(adapter, (data,), len(data), hashlib.sha256(data).hexdigest())

    assert asyncio.run(read_all(adapter, KEY, 2, 5)) == b"2345"
    assert asyncio.run(read_all(adapter, KEY, 7)) == b"789"


def test_delete_returns_freed_bytes_and_is_idempotent(adapter: LocalFsStorageAdapter):
    put(adapter, (b"abc",), 3, hashlib.sha256(b"abc").hexdigest())

    assert asyncio.run(adapter.delete(KEY)) == 3
    assert asyncio.run(adapter.delete(KEY)) == 0


def test_rejects_keys_that_could_escape_blob_dir(adapter: LocalFsStorageAdapter):
    with pytest.raises(ValueError):
        asyncio.run(adapter.delete("../../etc/passwd"))


def test_restart_purges_uploads_staged_before_a_crash(tmp_path: Path):
    LocalFsStorageAdapter(tmp_path)
    (tmp_path / "tmp" / "crashed-upload").write_bytes(b"half")

    LocalFsStorageAdapter(tmp_path)

    assert list((tmp_path / "tmp").iterdir()) == []


def test_build_storage_selects_driver_from_settings(tmp_path: Path):
    local = build_storage(Settings(data_dir=tmp_path))
    s3 = build_storage(
        Settings(data_dir=tmp_path, storage_driver="s3", s3=S3Settings(endpoint_url="http://127.0.0.1:8333"))
    )

    assert local.driver_name == "local-fs"
    assert s3.driver_name == "s3"
    assert isinstance(local, StorageAdapter) and isinstance(s3, StorageAdapter)


def test_s3_stub_requires_endpoint_and_does_not_pretend_to_store():
    with pytest.raises(ValueError):
        S3StorageAdapter(S3Settings())
    with pytest.raises(NotImplementedError):
        asyncio.run(S3StorageAdapter(S3Settings(endpoint_url="http://garage:3900")).delete(KEY))

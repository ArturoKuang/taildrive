"""The blob storage contract. Drivers know nothing about drive paths or quotas."""

from collections.abc import AsyncIterable, AsyncIterator
from dataclasses import dataclass
from typing import Protocol, runtime_checkable


@dataclass(frozen=True)
class StoredBlob:
    key: str
    size_bytes: int
    sha256: str


class BlobMismatch(Exception):
    """The uploaded bytes did not match the declared size or SHA-256. Nothing was stored."""

    def __init__(self, *, expected_size: int, actual_size: int, expected_sha256: str, actual_sha256: str | None):
        super().__init__("uploaded content does not match declared size or SHA-256")
        self.expected_size = expected_size
        self.actual_size = actual_size
        self.expected_sha256 = expected_sha256
        self.actual_sha256 = actual_sha256


@runtime_checkable
class StorageAdapter(Protocol):
    driver_name: str

    async def put(
        self, key: str, chunks: AsyncIterable[bytes], expected_size: int, expected_sha256: str
    ) -> StoredBlob:
        """Stage, hash and verify, then commit `key` atomically.

        Raises BlobMismatch (after discarding the staged bytes) when the stream's
        length or SHA-256 differs from what was declared. Any other failure also
        leaves no staged bytes behind.
        """
        ...

    def get(self, key: str, start: int = 0, end: int | None = None) -> AsyncIterator[bytes]:
        """Stream bytes `start..end` inclusive (`end=None` reads to the end)."""
        ...

    async def delete(self, key: str) -> int:
        """Remove `key` and return the bytes freed; a missing key frees 0."""
        ...

    async def disk_free_bytes(self) -> int:
        """Free bytes on the backing volume, used for the hard free-space floor."""
        ...

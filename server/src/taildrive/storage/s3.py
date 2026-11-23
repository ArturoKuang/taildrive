"""S3-compatible driver for SeaweedFS / Garage. Slice 1 ships the shape only.

Implementation plan (so the swap needs zero API or client changes):
- put: multipart upload to a staging key while hashing; on size/SHA-256
  mismatch AbortMultipartUpload and raise BlobMismatch; on success
  CompleteMultipartUpload with `x-amz-checksum-sha256`, then CopyObject to `key`.
- get: GetObject with `Range: bytes=start-end`.
- delete: HeadObject for the size, then DeleteObject.
- disk_free_bytes: cluster free space from the SeaweedFS master / Garage admin API.
"""

from collections.abc import AsyncIterable, AsyncIterator

from taildrive.config import S3Settings
from taildrive.storage.base import StoredBlob


class S3StorageAdapter:
    driver_name = "s3"

    def __init__(self, settings: S3Settings) -> None:
        if not settings.endpoint_url:
            raise ValueError("TAILDRIVE_S3_ENDPOINT_URL is required for the s3 driver")
        self.settings = settings

    async def put(
        self, key: str, chunks: AsyncIterable[bytes], expected_size: int, expected_sha256: str
    ) -> StoredBlob:
        raise NotImplementedError("s3 driver lands after Slice 1")

    def get(self, key: str, start: int = 0, end: int | None = None) -> AsyncIterator[bytes]:
        raise NotImplementedError("s3 driver lands after Slice 1")

    async def delete(self, key: str) -> int:
        raise NotImplementedError("s3 driver lands after Slice 1")

    async def disk_free_bytes(self) -> int:
        raise NotImplementedError("s3 driver lands after Slice 1")

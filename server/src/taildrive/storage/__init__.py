from taildrive.config import Settings
from taildrive.storage.base import BlobMismatch, StorageAdapter, StoredBlob
from taildrive.storage.local_fs import LocalFsStorageAdapter
from taildrive.storage.s3 import S3StorageAdapter

__all__ = [
    "BlobMismatch",
    "LocalFsStorageAdapter",
    "S3StorageAdapter",
    "StorageAdapter",
    "StoredBlob",
    "build_storage",
]


def build_storage(settings: Settings) -> StorageAdapter:
    if settings.storage_driver == "local-fs":
        return LocalFsStorageAdapter(settings.data_dir)
    if settings.storage_driver == "s3":
        return S3StorageAdapter(settings.s3)
    raise ValueError(f"unknown TAILDRIVE_STORAGE_DRIVER: {settings.storage_driver!r}")

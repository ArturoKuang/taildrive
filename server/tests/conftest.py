import hashlib
from collections.abc import Callable
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from taildrive.app import create_app
from taildrive.config import Settings

TOKEN = "test-token-abc123"
AUTH = {"Authorization": f"Bearer {TOKEN}"}

MakeClient = Callable[..., TestClient]


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@pytest.fixture
def data_dir(tmp_path: Path) -> Path:
    return tmp_path / "data"


@pytest.fixture
def make_client(data_dir: Path) -> MakeClient:
    """Build a client against a real LocalFs data dir; defaults leave quota and floor out of the way."""

    def make(*, quota_bytes: int = 10_000, free_floor_bytes: int = 0, client_host: str = "testclient") -> TestClient:
        settings = Settings(
            data_dir=data_dir,
            quota_bytes=quota_bytes,
            free_floor_bytes=free_floor_bytes,
            api_tokens=frozenset({TOKEN}),
        )
        return TestClient(create_app(settings), client=(client_host, 50000))

    return make


@pytest.fixture
def client(make_client: MakeClient) -> TestClient:
    return make_client()


def upload(client: TestClient, path: str, data: bytes, *, digest: str | None = None, **headers: str):
    return client.put(
        "/v1/files",
        params={"path": path},
        content=data,
        headers={**AUTH, "X-Content-SHA256": digest or sha(data), **headers},
    )


def stored_blobs(data_dir: Path) -> list[Path]:
    return [p for p in (data_dir / "blobs").rglob("*") if p.is_file()]


def staged_files(data_dir: Path) -> list[Path]:
    return list((data_dir / "tmp").iterdir())

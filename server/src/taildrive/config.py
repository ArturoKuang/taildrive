"""Runtime settings, parsed once from the environment at startup."""

import os
from dataclasses import dataclass, field
from pathlib import Path

FIVE_GB = 5 * 1024**3


@dataclass(frozen=True)
class S3Settings:
    endpoint_url: str | None = None
    bucket: str = "taildrive"
    region: str = "us-east-1"
    access_key_id: str | None = None
    secret_access_key: str | None = None


@dataclass(frozen=True)
class Settings:
    data_dir: Path
    quota_bytes: int = FIVE_GB
    free_floor_bytes: int = FIVE_GB
    api_tokens: frozenset[str] = frozenset()
    node_name: str = "macmini-ci"
    storage_driver: str = "local-fs"
    s3: S3Settings = field(default_factory=S3Settings)

    @property
    def db_path(self) -> Path:
        return self.data_dir / "taildrive.db"

    @classmethod
    def from_env(cls, env: dict[str, str] | None = None) -> "Settings":
        env = dict(os.environ if env is None else env)
        tokens = frozenset(t.strip() for t in env.get("TAILDRIVE_API_TOKENS", "").split(",") if t.strip())
        return cls(
            data_dir=Path(env.get("TAILDRIVE_DATA_DIR", "./data")).expanduser().resolve(),
            quota_bytes=_non_negative_int(env, "TAILDRIVE_QUOTA_BYTES", FIVE_GB),
            free_floor_bytes=_non_negative_int(env, "TAILDRIVE_FREE_FLOOR_BYTES", FIVE_GB),
            api_tokens=tokens,
            node_name=env.get("TAILDRIVE_NODE_NAME", "macmini-ci"),
            storage_driver=env.get("TAILDRIVE_STORAGE_DRIVER", "local-fs"),
            s3=S3Settings(
                endpoint_url=env.get("TAILDRIVE_S3_ENDPOINT_URL"),
                bucket=env.get("TAILDRIVE_S3_BUCKET", "taildrive"),
                region=env.get("TAILDRIVE_S3_REGION", "us-east-1"),
                access_key_id=env.get("TAILDRIVE_S3_ACCESS_KEY_ID"),
                secret_access_key=env.get("TAILDRIVE_S3_SECRET_ACCESS_KEY"),
            ),
        )


def _non_negative_int(env: dict[str, str], name: str, default: int) -> int:
    raw = env.get(name)
    if raw is None:
        return default
    value = int(raw)
    if value < 0:
        raise ValueError(f"{name} must be >= 0, got {value}")
    return value

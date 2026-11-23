"""`uv run taildrive-server`: bind loopback so only `tailscale serve` can front it."""

import os

import uvicorn

from taildrive.app import create_app
from taildrive.config import Settings


def main() -> None:
    uvicorn.run(
        create_app(Settings.from_env()),
        host=os.environ.get("TAILDRIVE_HOST", "127.0.0.1"),
        port=int(os.environ.get("TAILDRIVE_PORT", "8787")),
    )


if __name__ == "__main__":
    main()

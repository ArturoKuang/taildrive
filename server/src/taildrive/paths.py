"""Parse client-supplied drive paths into a canonical form.

Drive paths are metadata only: blobs live under server-generated object keys,
so a user path never touches the filesystem and cannot escape through symlinks.
"""

import unicodedata

from taildrive.errors import ApiError

MAX_PATH_BYTES = 1024
ROOT = ""


def parse_path(raw: str | None, *, allow_root: bool = False) -> str:
    """Return the NFC-normalized relative path, or raise 400 ERR_INVALID_PATH.

    NFC matters: macOS hands out NFD filenames while Android and browsers send
    NFC, and both must address the same file.
    """
    if not raw:
        if allow_root:
            return ROOT
        raise _invalid(raw, "path is required")

    path = unicodedata.normalize("NFC", raw)
    if any(ch == "\\" or ord(ch) < 0x20 or ord(ch) == 0x7F for ch in path):
        raise _invalid(raw, "path contains a backslash or control character")
    if path.startswith("/"):
        raise _invalid(raw, "path must be relative")
    if any(segment in ("", ".", "..") for segment in path.split("/")):
        raise _invalid(raw, "path contains an empty, '.' or '..' segment")
    if len(path.encode()) > MAX_PATH_BYTES:
        raise _invalid(raw, f"path exceeds {MAX_PATH_BYTES} bytes")
    return path


def parent_of(path: str) -> str:
    return path.rpartition("/")[0]


def name_of(path: str) -> str:
    return path.rpartition("/")[2]


def ancestors_of(path: str) -> list[str]:
    """`a/b/c` -> [`a`, `a/b`]."""
    segments = path.split("/")[:-1]
    return ["/".join(segments[: i + 1]) for i in range(len(segments))]


def _invalid(raw: str | None, reason: str) -> ApiError:
    return ApiError(400, "ERR_INVALID_PATH", reason, details={"path": raw})

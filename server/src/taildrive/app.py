"""HTTP boundary: parse requests, call Drive, render JSON and the error envelope."""

import base64
import binascii
import mimetypes
import re
from dataclasses import asdict
from typing import Annotated, Any
from urllib.parse import quote

from fastapi import APIRouter, Depends, FastAPI, Query, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse, StreamingResponse
from pydantic import BaseModel
from starlette.exceptions import HTTPException as StarletteHTTPException

from taildrive.auth import Principal, authenticate
from taildrive.config import Settings
from taildrive.drive import Drive
from taildrive.errors import ApiError, envelope
from taildrive.metadata import Entry, MetadataStore
from taildrive.paths import parse_path
from taildrive.storage import StorageAdapter, build_storage

_SHA256 = re.compile(r"[0-9a-f]{64}")
_RANGE = re.compile(r"bytes=(\d*)-(\d*)")


class FolderCreate(BaseModel):
    path: str


def create_app(settings: Settings, storage: StorageAdapter | None = None) -> FastAPI:
    storage = storage or build_storage(settings)
    drive = Drive(
        MetadataStore(settings.db_path),
        storage,
        quota_bytes=settings.quota_bytes,
        free_floor_bytes=settings.free_floor_bytes,
    )

    def principal(request: Request) -> Principal:
        return authenticate(request, settings.api_tokens)

    router = APIRouter(prefix="/v1", dependencies=[Depends(principal)])
    Caller = Annotated[Principal, Depends(principal)]

    @router.get("/health")
    async def health(caller: Caller) -> dict[str, Any]:
        return {
            "status": "ok",
            "node": settings.node_name,
            "tailnet_user": caller.tailnet_user,
            "storage_driver": storage.driver_name,
            "quota": asdict(await drive.quota()),
        }

    @router.get("/files")
    async def list_files(
        path: str | None = None,
        cursor: str | None = None,
        limit: Annotated[int, Query(ge=1, le=1000)] = 100,
    ) -> dict[str, Any]:
        folder = parse_path(path, allow_root=True)
        items, next_after = drive.list_folder(folder, after=_decode_cursor(cursor), limit=limit)
        return {
            "path": folder,
            "items": [_item(e) for e in items],
            "next_cursor": _encode_cursor(next_after) if next_after else None,
        }

    @router.post("/folders", status_code=201)
    async def create_folder(body: FolderCreate) -> dict[str, Any]:
        return _item(drive.create_folder(parse_path(body.path)))

    @router.put("/files")
    async def put_file(request: Request, path: str | None = None) -> JSONResponse:
        file_path = parse_path(path)
        sha256 = request.headers.get("x-content-sha256", "").strip().lower()
        if not _SHA256.fullmatch(sha256):
            raise ApiError(400, "ERR_BAD_REQUEST", "X-Content-SHA256 must be 64 hex characters")
        raw_length = request.headers.get("content-length")
        if raw_length is None:
            raise ApiError(411, "ERR_LENGTH_REQUIRED", "Content-Length is required")
        if not raw_length.isdigit():
            raise ApiError(400, "ERR_BAD_REQUEST", "Content-Length must be a non-negative integer")
        mime = request.headers.get("content-type") or mimetypes.guess_type(file_path)[0] or "application/octet-stream"

        entry, created = await drive.put_file(
            file_path,
            request.stream(),
            size=int(raw_length),
            sha256=sha256,
            mime=mime,
            if_match=request.headers.get("if-match"),
        )
        body = {"item": _item(entry), "quota": asdict(await drive.quota())}
        return JSONResponse(body, status_code=201 if created else 200)

    @router.get("/files/content")
    async def get_content(request: Request, path: str | None = None) -> StreamingResponse:
        entry = drive.read_file(parse_path(path))
        assert entry.object_key is not None and entry.sha256 is not None
        headers = {
            "ETag": entry.etag or "",
            "X-Content-SHA256": entry.sha256,
            "Content-Disposition": f"attachment; filename*=UTF-8''{quote(entry.name)}",
            "Accept-Ranges": "bytes",
        }
        byte_range = _parse_range(request.headers.get("range"), entry.size_bytes)
        if byte_range is None:
            headers["Content-Length"] = str(entry.size_bytes)
            return StreamingResponse(storage.get(entry.object_key), media_type=entry.mime, headers=headers)
        start, end = byte_range
        headers["Content-Length"] = str(end - start + 1)
        headers["Content-Range"] = f"bytes {start}-{end}/{entry.size_bytes}"
        return StreamingResponse(
            storage.get(entry.object_key, start, end), status_code=206, media_type=entry.mime, headers=headers
        )

    @router.delete("/files")
    async def delete_file(path: str | None = None, recursive: bool = False) -> dict[str, Any]:
        deleted = await drive.delete(parse_path(path), recursive=recursive)
        return {
            "deleted": True,
            "path": deleted.path,
            "freed_bytes": deleted.freed_bytes,
            "quota": asdict(await drive.quota()),
        }

    app = FastAPI(title="TailDrive", version="0.1.0")
    app.include_router(router)
    _install_error_handlers(app)
    return app


def _item(entry: Entry) -> dict[str, Any]:
    return {
        "id": entry.id,
        "name": entry.name,
        "path": entry.path,
        "type": entry.type,
        "size_bytes": entry.size_bytes,
        "mime": entry.mime,
        "sha256": entry.sha256,
        "version": entry.version,
        "etag": entry.etag,
        "updated_at": entry.updated_at,
    }


def _encode_cursor(after: str) -> str:
    return base64.urlsafe_b64encode(after.encode()).decode()


def _decode_cursor(cursor: str | None) -> str | None:
    if not cursor:
        return None
    try:
        return base64.b64decode(cursor.encode(), altchars=b"-_", validate=True).decode()
    except (binascii.Error, UnicodeDecodeError, ValueError):
        raise ApiError(400, "ERR_BAD_REQUEST", "invalid cursor") from None


def _parse_range(header: str | None, size: int) -> tuple[int, int] | None:
    """Single `bytes=` range -> inclusive (start, end); None means the whole file."""
    if header is None:
        return None
    match = _RANGE.fullmatch(header.strip())
    if match and size > 0:
        first, last = match.groups()
        if first:
            start, end = int(first), min(int(last), size - 1) if last else size - 1
        elif last:
            start, end = max(size - int(last), 0), size - 1
        else:
            start, end = 0, -1
        if start <= end and start < size:
            return start, end
    raise ApiError(
        416, "ERR_RANGE_NOT_SATISFIABLE", "Range is not satisfiable", details={"range": header, "size_bytes": size}
    )


def _install_error_handlers(app: FastAPI) -> None:
    @app.exception_handler(ApiError)
    async def api_error(_: Request, exc: ApiError) -> JSONResponse:
        return JSONResponse(exc.envelope(), status_code=exc.status)

    @app.exception_handler(RequestValidationError)
    async def validation_error(_: Request, exc: RequestValidationError) -> JSONResponse:
        errors = [{"loc": list(e["loc"]), "msg": e["msg"]} for e in exc.errors()]
        return JSONResponse(
            envelope("ERR_BAD_REQUEST", "request validation failed", details={"errors": errors}), status_code=400
        )

    @app.exception_handler(StarletteHTTPException)
    async def http_error(_: Request, exc: StarletteHTTPException) -> JSONResponse:
        codes = {404: "ERR_NOT_FOUND", 405: "ERR_METHOD_NOT_ALLOWED"}
        return JSONResponse(
            envelope(codes.get(exc.status_code, f"ERR_HTTP_{exc.status_code}"), str(exc.detail)),
            status_code=exc.status_code,
            headers=exc.headers,
        )

    @app.exception_handler(Exception)
    async def internal_error(_: Request, exc: Exception) -> JSONResponse:
        return JSONResponse(envelope("ERR_INTERNAL", "internal server error", retryable=True), status_code=500)

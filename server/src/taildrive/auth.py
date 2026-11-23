"""Who is calling: a Tailscale identity via `tailscale serve`, or a bearer token."""

import hmac
import ipaddress
from dataclasses import dataclass

from fastapi import Request

from taildrive.errors import ApiError


@dataclass(frozen=True)
class Principal:
    tailnet_user: str | None


def authenticate(request: Request, api_tokens: frozenset[str]) -> Principal:
    authorization = request.headers.get("authorization")
    if authorization is not None:
        scheme, _, token = authorization.partition(" ")
        if scheme.lower() == "bearer" and _token_valid(token.strip(), api_tokens):
            return Principal(tailnet_user=request.headers.get("tailscale-user-login") if _from_loopback(request) else None)
        raise _unauthorized("invalid bearer token")

    login = request.headers.get("tailscale-user-login")
    if login:
        # `tailscale serve` proxies from loopback; anyone else could forge the header.
        if _from_loopback(request):
            return Principal(tailnet_user=login)
        raise _unauthorized("Tailscale-User-Login is only trusted from loopback (tailscale serve)")
    raise _unauthorized("missing credentials")


def _token_valid(token: str, api_tokens: frozenset[str]) -> bool:
    # Compare against every token so timing doesn't reveal which one nearly matched.
    matched = False
    for candidate in api_tokens:
        matched |= hmac.compare_digest(token.encode(), candidate.encode())
    return bool(token) and matched


def _from_loopback(request: Request) -> bool:
    if request.client is None:
        return False
    try:
        return ipaddress.ip_address(request.client.host).is_loopback
    except ValueError:
        return False


def _unauthorized(message: str) -> ApiError:
    return ApiError(401, "ERR_UNAUTHORIZED", message)

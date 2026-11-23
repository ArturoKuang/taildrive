use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::Response,
};
use subtle::{Choice, ConstantTimeEq};
use taildrive_core::hash::sha256_hex;

use crate::{
    App,
    error::{ApiError, ApiResult},
};

#[derive(Clone)]
pub(crate) struct Principal {
    pub tailnet_user: Option<String>,
}

pub(crate) async fn require_auth(
    State(app): State<App>,
    mut request: Request,
    next: Next,
) -> ApiResult<Response> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let principal = authenticate(peer, request.headers(), &app.state.token_hashes)?;
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

fn authenticate(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    token_hashes: &[String],
) -> ApiResult<Principal> {
    if peer.is_some_and(|peer| peer.ip().to_canonical().is_loopback())
        && let Some(login) = headers
            .get("tailscale-user-login")
            .and_then(|value| value.to_str().ok())
        && !login.trim().is_empty()
    {
        return Ok(Principal {
            tailnet_user: Some(login.trim().to_owned()),
        });
    }
    if let Some((scheme, token)) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        && scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
    {
        let candidate = sha256_hex(token);
        let mut matched = Choice::from(0);
        for hash in token_hashes {
            matched |= hash.as_bytes().ct_eq(candidate.as_bytes());
        }
        if bool::from(matched) {
            return Ok(Principal { tailnet_user: None });
        }
    }
    Err(ApiError::new(
        StatusCode::UNAUTHORIZED,
        "ERR_UNAUTHORIZED",
        "authentication required",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_loopback_is_trusted_but_missing_connect_info_is_not() {
        let mut headers = HeaderMap::new();
        headers.insert("tailscale-user-login", "alice@ts.net".parse().unwrap());
        headers.insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
        assert!(
            authenticate(
                Some("[::ffff:127.0.0.1]:15".parse().unwrap()),
                &headers,
                &[]
            )
            .is_ok()
        );
        assert!(authenticate(None, &headers, &[]).is_err());
        assert!(
            authenticate(
                Some("[::ffff:192.0.2.10]:15".parse().unwrap()),
                &headers,
                &[]
            )
            .is_err()
        );
    }
}

use std::ops::Range;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};

use crate::{
    error::{ApiError, ApiResult},
    storage::ByteStream,
};

pub(crate) fn byte_range(headers: &HeaderMap, size: u64) -> ApiResult<Option<Range<u64>>> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(ApiError::range(size));
    }
    let value = value.to_str().map_err(|_| ApiError::range(size))?;
    parse_range(value, size)
        .map(Some)
        .ok_or_else(|| ApiError::range(size))
}

fn parse_range(value: &str, size: u64) -> Option<Range<u64>> {
    let (unit, bounds) = value.trim().split_once('=')?;
    if size == 0 || !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (first, last) = bounds.split_once('-')?;
    let start = if first.is_empty() {
        size.saturating_sub(decimal(last)?)
    } else {
        decimal(first)?
    };
    let end = if first.is_empty() || last.is_empty() {
        size
    } else {
        decimal(last)?.saturating_add(1).min(size)
    };
    (start < end).then_some(start..end)
}

fn decimal(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // A huge suffix/end may still select a valid range.
    Some(s.bytes().fold(0_u64, |n, b| {
        n.saturating_mul(10).saturating_add(u64::from(b - b'0'))
    }))
}

fn filename_parameter(name: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::from("attachment; filename*=UTF-8''");
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(char::from(b));
        } else {
            write!(encoded, "%{b:02X}").expect("writing to a String");
        }
    }
    encoded
}

pub(crate) fn download(
    entry: crate::db::Entry,
    range: Option<Range<u64>>,
    stream: ByteStream,
) -> ApiResult<Response> {
    let item = entry.item;
    let mut response = Response::builder()
        .header(
            header::CONTENT_TYPE,
            item.mime.as_deref().unwrap_or("application/octet-stream"),
        )
        .header(header::ETAG, item.etag.as_deref().unwrap_or_default())
        .header(
            "x-content-sha256",
            item.sha256.as_deref().unwrap_or_default(),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_DISPOSITION, filename_parameter(&item.name))
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
    response = if let Some(range) = range {
        response
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                header::CONTENT_RANGE,
                format!(
                    "bytes {}-{}/{}",
                    range.start,
                    range.end - 1,
                    item.size_bytes
                ),
            )
            .header(header::CONTENT_LENGTH, range.end - range.start)
    } else {
        response.header(header::CONTENT_LENGTH, item.size_bytes)
    };
    response
        .body(Body::from_stream(stream))
        .map_err(ApiError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_edges() {
        assert_eq!(parse_range("bytes=2-4", 10), Some(2..5));
        assert_eq!(parse_range("bytes=2-", 10), Some(2..10));
        assert_eq!(parse_range("bytes=-2", 10), Some(8..10));
        assert_eq!(
            parse_range("bytes=1-9999999999999999999999", 10),
            Some(1..10)
        );
        assert_eq!(
            parse_range("bytes=-9999999999999999999999", 10),
            Some(0..10)
        );
        for value in [
            "bytes=10-",
            "bytes=-0",
            "bytes=-",
            "bytes=+1-2",
            "bytes=1-2,3-4",
            "bytes=4-3",
            "chars=0-1",
        ] {
            assert_eq!(parse_range(value, 10), None, "{value}");
        }
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }
}

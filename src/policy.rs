use http::{header, HeaderMap, HeaderValue, Method, StatusCode};

pub fn is_playlist_type(headers: &HeaderMap) -> bool {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "audio/mpegurl" | "audio/x-mpegurl"))
}

pub fn prepare_request(headers: &mut HeaderMap, playlist: bool) {
    // Also covers extensionless playlists discovered by Content-Type later.
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if playlist {
        // Byte ranges and validators from the original representation do not
        // describe the rewritten representation; fetch a complete fresh list.
        for key in ["range", "if-range", "if-none-match", "if-modified-since", "if-match", "if-unmodified-since"] {
            headers.remove(key);
        }
    }
}

/// Must run in upstream_response_filter, BEFORE Pingora selects downstream
/// framing. Never insert Transfer-Encoding here: HTTP/2 forbids that header.
/// Pingora adds HTTP/1.1 chunked framing automatically once CL is absent.
pub fn prepare_response(headers: &mut HeaderMap, status: StatusCode, method: &Method, playlist_hint: bool, jpeg: bool) -> Result<bool, String> {
    let playlist = playlist_hint || is_playlist_type(headers);
    if playlist && status == StatusCode::PARTIAL_CONTENT {
        return Err("Cannot rewrite a partial playlist; request the full playlist without Range".into());
    }
    let rewrite = playlist && status == StatusCode::OK;
    if rewrite {
        if headers.get(header::CONTENT_ENCODING).is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity")) {
            return Err("Upstream ignored Accept-Encoding: identity for playlist; compressed rewriting is not supported".into());
        }
        for key in ["content-length", "content-encoding", "content-range", "accept-ranges", "etag", "last-modified", "content-md5", "digest", "content-digest", "repr-digest", "content-disposition", "trailer"] {
            headers.remove(key);
        }
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    } else if jpeg && status.is_success() {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp2t"));
        headers.remove(header::CONTENT_DISPOSITION);
    }
    Ok(rewrite && method != Method::HEAD)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_length_removed_before_framing_and_validators_cleared() {
        let mut h = HeaderMap::new();
        for (k, v) in [("content-length", "42"), ("etag", "\"upstream\""), ("content-type", "text/plain"), ("content-range", "bytes 0-41/42")] { h.insert(header::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap()); }
        assert!(prepare_response(&mut h, StatusCode::OK, &Method::GET, true, false).unwrap());
        for k in ["content-length", "etag", "content-range", "transfer-encoding"] { assert!(!h.contains_key(k)); }
        assert_eq!(h[header::CONTENT_TYPE], "application/vnd.apple.mpegurl");
    }
    #[test]
    fn mime_detection_and_compression_rejection() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, "Application/Vnd.Apple.MpegURL; charset=utf-8".parse().unwrap());
        assert!(prepare_response(&mut h, StatusCode::OK, &Method::GET, false, false).unwrap());
        h.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        assert!(prepare_response(&mut h, StatusCode::OK, &Method::GET, true, false).is_err());
    }
    #[test]
    fn error_redirect_no_body_and_media_range_not_rewritten() {
        for status in [StatusCode::FOUND, StatusCode::NOT_MODIFIED, StatusCode::NO_CONTENT, StatusCode::FORBIDDEN, StatusCode::NOT_FOUND] {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_LENGTH, "123".parse().unwrap());
            assert!(!prepare_response(&mut h, status, &Method::GET, true, false).unwrap());
            assert_eq!(h[header::CONTENT_LENGTH], "123");
        }
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_RANGE, "bytes 0-9/100".parse().unwrap());
        assert!(!prepare_response(&mut h, StatusCode::PARTIAL_CONTENT, &Method::GET, false, true).unwrap());
        assert_eq!(h[header::CONTENT_RANGE], "bytes 0-9/100");
        assert!(prepare_response(&mut h, StatusCode::PARTIAL_CONTENT, &Method::GET, true, false).is_err());
        assert!(!prepare_response(&mut h, StatusCode::OK, &Method::HEAD, true, false).unwrap());
    }
    #[test]
    fn playlist_request_strips_range_but_media_keeps_it() {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, "bytes=0-99".parse().unwrap());
        h.insert(header::IF_NONE_MATCH, "\"upstream\"".parse().unwrap());
        prepare_request(&mut h, false);
        assert!(h.contains_key(header::RANGE));
        prepare_request(&mut h, true);
        assert!(!h.contains_key(header::RANGE));
        assert!(!h.contains_key(header::IF_NONE_MATCH));
        assert_eq!(h[header::ACCEPT_ENCODING], "identity");
    }
}

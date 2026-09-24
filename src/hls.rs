use bytes::Bytes;
use url::Url;

pub fn is_playlist_url(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u")
}

pub fn proxy_url(base: &str, target: &Url) -> String {
    format!("{base}?url={}", urlencoding::encode(target.as_str()))
}

/// Remove only our compatibility marker, preserving signed query bytes exactly.
/// Old clients may still use fake .ts URLs; new links retain the real .jpeg path.
pub fn parse_target(encoded: &str) -> Result<(Url, bool), String> {
    let decoded = urlencoding::decode(encoded).map_err(|_| "Invalid URL encoding")?;
    let mut url = Url::parse(&decoded).map_err(|_| "Invalid target URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some() {
        return Err("Target must be HTTP(S), have a host, and contain no credentials".into());
    }
    let jpeg = url.query_pairs().any(|(key, value)| key == "real_ext" && value == "jpeg");
    if jpeg {
        let clean = url.query().unwrap_or("").split('&').filter(|pair| {
            !url::form_urlencoded::parse(pair.as_bytes()).any(|(key, value)| key == "real_ext" && value == "jpeg")
        }).collect::<Vec<_>>().join("&");
        url.set_query(if clean.is_empty() { None } else { Some(&clean) });
        if let Some(stem) = url.path().strip_suffix(".ts") {
            let path = format!("{stem}.jpeg");
            url.set_path(&path);
        }
    }
    url.set_fragment(None);
    Ok((url, jpeg))
}

/// Incremental, bounded UTF-8 line rewriter. Network chunks are NOT text lines.
/// Only an incomplete line is retained; media bodies never enter this component.
pub struct PlaylistRewriter {
    target: Url,
    public_base: String,
    pending: Vec<u8>,
    total: usize,
    limit: usize,
    first_line: bool,
    finished: bool,
}

impl PlaylistRewriter {
    pub fn new(target: Url, public_base: String, limit: usize) -> Self {
        Self { target, public_base, pending: Vec::new(), total: 0, limit, first_line: true, finished: false }
    }

    pub fn push(&mut self, chunk: Option<Bytes>, end: bool) -> Result<Option<Bytes>, String> {
        if self.finished {
            return if chunk.as_ref().is_none_or(|b| b.is_empty()) { Ok(None) } else { Err("Playlist bytes received after EOF".into()) };
        }
        if let Some(bytes) = chunk {
            self.total = self.total.checked_add(bytes.len()).ok_or("Playlist size overflow")?;
            if self.total > self.limit { return Err(format!("Playlist exceeds MAX_PLAYLIST_BYTES ({})", self.limit)); }
            self.pending.extend_from_slice(&bytes);
        }
        let complete = if end { self.pending.len() } else {
            self.pending.iter().rposition(|b| *b == b'\n').map_or(0, |pos| pos + 1)
        };
        if complete == 0 {
            self.finished = end;
            return Ok(None);
        }
        let text = std::str::from_utf8(&self.pending[..complete]).map_err(|_| "Playlist is not valid UTF-8")?;
        let mut output = String::new();
        for line in text.lines() {
            let line = if self.first_line { line.trim_start_matches('\u{feff}') } else { line };
            self.first_line = false;
            output.push_str(&rewrite_line(line, &self.target, &self.public_base));
            output.push('\n');
        }
        self.pending.drain(..complete);
        self.finished = end;
        Ok((!output.is_empty()).then(|| Bytes::from(output)))
    }
}

fn rewrite_line(line: &str, target: &Url, public_base: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() { return String::new(); }
    if trimmed.starts_with('#') {
        // Comments and tag attributes remain intact, including EXTINF titles.
        if trimmed.starts_with("#EXT-X-") {
            return rewrite_attributes(line, target, public_base);
        }
        return line.to_string();
    }
    // Every non-comment URI is valid in an M3U playlist. Never discard numeric
    // filenames, extensionless endpoints, query-only URIs or custom schemes.
    rewrite_uri(trimmed, target, public_base, true)
}

fn rewrite_uri(uri: &str, target: &Url, public_base: &str, jpeg_segment: bool) -> String {
    // Variable substitution and DRM/data schemes require player interpretation.
    // Keep them untouched rather than URL-encoding away their semantics.
    if uri.is_empty() || uri.contains("{$") { return uri.to_string(); }
    let Ok(mut resolved) = target.join(uri) else { return uri.to_string(); };
    if !matches!(resolved.scheme(), "http" | "https") { return uri.to_string(); }
    if jpeg_segment && resolved.path().to_ascii_lowercase().ends_with(".jpeg")
        && !resolved.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg") {
        let query = match resolved.query() {
            Some(q) if !q.is_empty() => format!("{q}&real_ext=jpeg"),
            _ => "real_ext=jpeg".to_string(),
        };
        resolved.set_query(Some(&query));
    }
    proxy_url(public_base, &resolved)
}

/// Scan an HLS attribute-list without splitting commas inside quoted strings.
/// Exact URI keys only: do not rewrite arbitrary metadata containing "URI=".
fn rewrite_attributes(line: &str, target: &Url, public_base: &str) -> String {
    let Some(colon) = line.find(':') else { return line.into(); };
    let mut result = line[..=colon].to_string();
    let attributes = &line[colon + 1..];
    let mut start = 0;
    let mut quoted = false;
    for (pos, byte) in attributes.bytes().enumerate() {
        if byte == b'"' { quoted = !quoted; }
        if byte == b',' && !quoted {
            result.push_str(&rewrite_attribute(&attributes[start..pos], target, public_base));
            result.push(',');
            start = pos + 1;
        }
    }
    result.push_str(&rewrite_attribute(&attributes[start..], target, public_base));
    result
}

fn rewrite_attribute(attribute: &str, target: &Url, public_base: &str) -> String {
    let Some(eq) = attribute.find('=') else { return attribute.into(); };
    let key = attribute[..eq].trim();
    if !matches!(key, "URI" | "SERVER-URI") { return attribute.into(); }
    let raw = &attribute[eq + 1..];
    let value = raw.trim();
    let Some(uri) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else { return attribute.into(); };
    let leading = &raw[..raw.len() - raw.trim_start().len()];
    let trailing = &raw[raw.trim_end().len()..];
    format!("{}={leading}\"{}\"{trailing}", &attribute[..eq], rewrite_uri(uri, target, public_base, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: &str = "http://127.0.0.1:8080/";
    fn origin() -> Url { Url::parse("https://cdn.example/a/b/list.m3u8?auth=secret").unwrap() }
    fn rewrite(input: &[u8], chunks: &[usize]) -> String {
        let mut r = PlaylistRewriter::new(origin(), BASE.into(), 1024 * 1024);
        let mut output = Vec::new();
        let mut offset = 0;
        for &len in chunks {
            let next = (offset + len).min(input.len());
            if let Some(bytes) = r.push(Some(Bytes::copy_from_slice(&input[offset..next])), false).unwrap() { output.extend_from_slice(&bytes); }
            offset = next;
        }
        if let Some(bytes) = r.push(Some(Bytes::copy_from_slice(&input[offset..])), true).unwrap() { output.extend_from_slice(&bytes); }
        String::from_utf8(output).unwrap()
    }
    fn proxied(relative: &str) -> String { proxy_url(BASE, &origin().join(relative).unwrap()) }
    #[test]
    fn every_possible_boundary_including_utf8_and_crlf() {
        let input = "\u{feff}#EXTM3U\r\n#EXTINF:4,测试\r\n00001.ts\r\n../next?sig=a%2Fb+c\r\n#EXT-X-ENDLIST".as_bytes();
        let expected = rewrite(input, &[]);
        for split in 0..=input.len() { assert_eq!(rewrite(input, &[split]), expected, "split at {split}"); }
        assert_eq!(rewrite(input, &vec![1; input.len()]), expected);
        assert!(expected.contains(&proxied("00001.ts")));
        assert!(expected.contains("#EXTINF:4,测试\n"));
        assert!(!expected.starts_with('\u{feff}'));
    }
    #[test]
    fn all_uri_forms_and_tags_preserved() {
        let uris = ["00001.ts", "42", "segment?id=1", "../next.ts", "/root.ts", "//other.example/seg.ts", "?part=2", "https://other.example/s.ts"];
        for uri in uris { assert_eq!(rewrite(format!("#EXTM3U\n{uri}").as_bytes(), &[]), format!("#EXTM3U\n{}\n", proxied(uri))); }
        assert_eq!(rewrite(b"#EXTM3U\n#EXTINF:1,\n#comment\n00001.ts\n", &[]), format!("#EXTM3U\n#EXTINF:1,\n#comment\n{}\n", proxied("00001.ts")));
    }
    #[test]
    fn embedded_uri_tags_and_quoted_commas() {
        for tag in ["KEY", "SESSION-KEY", "MAP", "MEDIA", "I-FRAME-STREAM-INF", "PART", "PRELOAD-HINT", "RENDITION-REPORT"] {
            let input = format!("#EXTM3U\n#EXT-X-{tag}:NAME=\"a,b URI=foo\",URI=\"../key?id=x,y\",X-URI=\"leave\"\n");
            let out = rewrite(input.as_bytes(), &[21, 13]);
            assert!(out.contains(&format!("URI=\"{}\"", proxied("../key?id=x,y"))), "{tag}: {out}");
            assert!(out.contains("NAME=\"a,b URI=foo\""));
            assert!(out.contains("X-URI=\"leave\""));
        }
        let out = rewrite(b"#EXTM3U\n#EXT-X-CONTENT-STEERING:SERVER-URI=\"/steer\"\n", &[]);
        assert!(out.contains(&proxied("/steer")));
    }
    #[test]
    fn eof_without_body_flushes_pending_line() {
        let mut r = PlaylistRewriter::new(origin(), BASE.into(), 100);
        assert!(r.push(Some(Bytes::from_static(b"0001.ts")), false).unwrap().is_none());
        assert_eq!(r.push(None, true).unwrap().unwrap(), format!("{}\n", proxied("0001.ts")));
        assert!(r.push(None, true).unwrap().is_none());
    }
    #[test]
    fn bounded_buffer_and_invalid_utf8_fail_explicitly() {
        let mut r = PlaylistRewriter::new(origin(), BASE.into(), 4);
        assert!(r.push(Some(Bytes::from_static(b"12345")), false).is_err());
        let mut r = PlaylistRewriter::new(origin(), BASE.into(), 100);
        assert!(r.push(Some(Bytes::from_static(b"\xff\n")), true).is_err());
    }
    #[test]
    fn jpeg_query_roundtrip_keeps_signatures_and_path() {
        let raw = "https://cdn.example/a.ts/x.jpeg?sig=a%2fb+z&x=.ts&empty=";
        let out = rewrite(format!("#EXTM3U\n{raw}\n").as_bytes(), &[]);
        let encoded = out.lines().nth(1).unwrap().split_once("?url=").unwrap().1;
        let (target, jpeg) = parse_target(encoded).unwrap();
        assert!(jpeg);
        assert_eq!(target.as_str(), raw);
        let (target, jpeg) = parse_target(&urlencoding::encode("https://cdn.example/a.ts/s.ts?sig=a%2fb+z&x=.ts&real_ext=jpeg")).unwrap();
        assert!(jpeg);
        assert_eq!(target.as_str(), "https://cdn.example/a.ts/s.jpeg?sig=a%2fb+z&x=.ts");
    }
    #[test]
    fn custom_schemes_variables_and_comments_are_not_corrupted() {
        let input = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://license\"\n{$prefix}/seg.ts\ndata:abc\n#comment URI=\"abc\"\n";
        assert_eq!(rewrite(input.as_bytes(), &[]), input);
    }
    #[test]
    fn target_validation() {
        for raw in ["file:///etc/passwd", "ftp://example.com/a", "http://user:pass@example.com", "invalid"] {
            assert!(parse_target(&urlencoding::encode(raw)).is_err());
        }
        assert!(is_playlist_url(&Url::parse("https://cdn.example/LIST.M3U8?token=x").unwrap()));
    }
}

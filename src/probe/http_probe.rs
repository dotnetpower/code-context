use super::image_probe;
use super::json_probe::{
    JsonSummary, QueryFilters, parse_json_tolerant, summarize_parsed_value,
};
use super::{
    BBox, HttpProbeArgs, MAX_BATCH_URLS, Predicate, clamp_json_budget, clamp_utf8_bytes,
    percent_encode_component, redact_url, scrub_secrets, truncate_utf8_chars,
};
use crate::inline_budget::INLINE_BUDGET;
use crate::{Result, invalid};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_USER_AGENT: &str = "cctx-probe/0.26";

#[derive(Clone, Debug)]
pub struct RawHttpResponse {
    pub url: String,
    pub redacted_url: String,
    pub status: u16,
    pub content_type: String,
    pub headers: BTreeMap<String, String>,
    pub elapsed_ms: u64,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HttpEndpointSummary {
    pub label: String,
    pub url: String,
    pub ok: bool,
    pub status: u16,
    pub elapsed_ms: u64,
    pub bytes: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<JsonSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<image_probe::ImageInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xml: Option<XmlSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_preview: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct XmlSummary {
    pub root_tag: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub status_fields: BTreeMap<String, String>,
    pub item_tags: usize,
}

pub fn validate_http_url(url: &str) -> Result<(&str, &str)> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(invalid("URL must not be empty").into());
    }
    if trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid("URL must not contain whitespace or control characters").into());
    }
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err(invalid("only http:// and https:// URLs are permitted").into());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(invalid(format!("URL missing host: {trimmed}")).into());
    }
    Ok((scheme, rest))
}

fn validate_header_line(hdr: &str) -> Result<(String, String)> {
    if hdr.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        return Err(invalid("HTTP header must not contain CR/LF/NUL").into());
    }
    let Some((name, val)) = hdr.split_once(':') else {
        return Err(invalid(format!("HTTP header must use `Name: Value` format: {hdr}")).into());
    };
    let name = name.trim();
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid(format!("invalid HTTP header name: {name}")).into());
    }
    Ok((name.to_string(), val.trim().to_string()))
}

fn expand_url_templates(
    templates: &[String],
    raw_vars: &[String],
) -> Result<Vec<(String, String)>> {
    let mut vars: Vec<(String, Vec<String>)> = Vec::new();
    for spec in raw_vars {
        let Some((name, vals_str)) = spec.split_once('=') else {
            return Err(invalid(format!("--var must have form KEY=v1,v2 (got `{spec}`)")).into());
        };
        let key = name.trim();
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(invalid(format!("invalid --var name `{key}`")).into());
        }
        let values: Vec<String> = vals_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        if values.is_empty() {
            return Err(invalid(format!("--var `{key}` has no values")).into());
        }
        vars.push((key.to_string(), values));
    }

    let mut expanded: Vec<(String, String)> = Vec::new();
    for tmpl in templates {
        let mut current: Vec<(Vec<String>, String)> = vec![(Vec::new(), tmpl.clone())];
        for (k, values) in &vars {
            let token = format!("{{{k}}}");
            if !current.iter().any(|(_, u)| u.contains(&token)) {
                continue;
            }
            let mut next = Vec::new();
            for (labels, u) in current {
                for v in values {
                    let mut next_labels = labels.clone();
                    next_labels.push(format!("{k}={v}"));
                    next.push((next_labels, u.replace(&token, &percent_encode_path_or_query(v))));
                    if next.len() > MAX_BATCH_URLS {
                        return Err(invalid(format!(
                            "expanded URL batch exceeds limit of {MAX_BATCH_URLS}; reduce --var values"
                        ))
                        .into());
                    }
                }
            }
            current = next;
        }
        for (labels, u) in current {
            if let Some(unresolved) = find_unresolved_placeholder(&u) {
                return Err(invalid(format!(
                    "unresolved template placeholder `{{{unresolved}}}` in URL; supply `--var {unresolved}=...`"
                ))
                .into());
            }
            let label = if labels.is_empty() {
                short_endpoint_label(&u)
            } else {
                labels.join(",")
            };
            expanded.push((label, u));
            if expanded.len() > MAX_BATCH_URLS {
                return Err(invalid(format!(
                    "URL batch exceeds limit of {MAX_BATCH_URLS}"
                ))
                .into());
            }
        }
    }
    Ok(expanded)
}

fn percent_encode_path_or_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn find_unresolved_placeholder(url: &str) -> Option<String> {
    let bytes = url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(off) = url[i + 1..].find('}')
        {
            let name = &url[i + 1..i + 1 + off];
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                return Some(name.to_string());
            }
        }
        i += 1;
    }
    None
}

fn short_endpoint_label(url: &str) -> String {
    let redacted = redact_url(url, &[]);
    let without_scheme = redacted
        .strip_prefix("https://")
        .or_else(|| redacted.strip_prefix("http://"))
        .unwrap_or(&redacted);
    truncate_utf8_chars(without_scheme, 72)
}

fn is_loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn strip_sensitive_redirect_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| {
            let lower = k.to_ascii_lowercase();
            !matches!(lower.as_str(), "cookie" | "cookie2" | "proxy-authorization")
                && !super::is_sensitive_key_name(&lower)
        })
        .cloned()
        .collect()
}

/// Pure-Rust HTTP/1.1 client for loopback endpoints and offline integration tests.
fn fetch_via_rust_tcp(
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout_secs: u64,
    secrets: &[String],
) -> Result<RawHttpResponse> {
    let start = Instant::now();
    let mut current_url = url.to_string();
    let mut cur_method = method.to_string();
    let mut cur_headers: Vec<(String, String)> = headers.to_vec();
    let mut cur_body: Option<&[u8]> = body;
    let mut initial_authority: Option<String> = None;
    let timeout = Duration::from_secs(timeout_secs.clamp(1, 60));

    for _hop in 0..=5 {
        let (scheme, rest) = validate_http_url(&current_url)?;
        if scheme != "http" {
            let safe_headers = strip_sensitive_redirect_headers(&cur_headers);
            return fetch_via_curl(
                &current_url,
                &cur_method,
                &safe_headers,
                cur_body,
                timeout_secs,
                secrets,
            );
        }
        let (authority, path_and_query) = match rest.find(['/', '?']) {
            Some(idx) => {
                let pq = &rest[idx..];
                (&rest[..idx], if pq.starts_with('?') { format!("/{pq}") } else { pq.to_string() })
            }
            None => (rest, "/".to_string()),
        };
        if let Some(ref orig) = initial_authority {
            if !orig.eq_ignore_ascii_case(authority) {
                cur_headers = strip_sensitive_redirect_headers(&cur_headers);
            }
        } else {
            initial_authority = Some(authority.to_string());
        }
        let path_no_frag = path_and_query.split('#').next().unwrap_or("/");
        let host_port = if authority.contains(':') && !authority.ends_with(']') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };

        let addr: SocketAddr = host_port
            .to_socket_addrs()
            .map_err(|e| invalid(format!("DNS/socket resolve failed for {authority}: {e}")))?
            .next()
            .ok_or_else(|| invalid(format!("no socket address for {authority}")))?;

        let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        let mut req = format!("{cur_method} {path_no_frag} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
        let has_ua = cur_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));
        if !has_ua {
            req.push_str(&format!("User-Agent: {DEFAULT_USER_AGENT}\r\n"));
        }
        let has_accept = cur_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("accept"));
        if !has_accept {
            req.push_str("Accept: application/json, */*;q=0.8\r\n");
        }
        for (k, v) in &cur_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = cur_body {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");

        stream.write_all(req.as_bytes())?;
        if let Some(b) = cur_body {
            stream.write_all(b)?;
        }
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut line_buf = Vec::with_capacity(256);
        super::read_bounded_line(&mut reader, &mut line_buf, 8_192)?;
        let status_line = String::from_utf8_lossy(&line_buf);
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| invalid(format!("malformed HTTP status line: {}", status_line.trim())))?;

        let mut resp_headers = BTreeMap::new();
        for _hdr_idx in 0..128 {
            let n = super::read_bounded_line(&mut reader, &mut line_buf, 8_192)?;
            if n == 0 || line_buf == b"\r\n" || line_buf == b"\n" {
                break;
            }
            let line = String::from_utf8_lossy(&line_buf);
            if let Some((k, v)) = line.split_once(':') {
                resp_headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp_headers.get("location")
        {
            if (301..=303).contains(&status) && cur_method == "POST" {
                cur_method = "GET".to_string();
                cur_body = None;
            }
            current_url = resolve_redirect_url("http", authority, path_no_frag, loc);
            continue;
        }

        let mut resp_body = Vec::new();
        if cur_method != "HEAD" {
            if resp_headers
                .get("transfer-encoding")
                .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
            {
                resp_body = read_chunked_body(&mut reader, MAX_HTTP_BODY_BYTES)?;
            } else if let Some(cl) = resp_headers
                .get("content-length")
                .and_then(|v| v.parse::<usize>().ok())
            {
                let capped = cl.min(MAX_HTTP_BODY_BYTES);
                reader.take(capped as u64).read_to_end(&mut resp_body)?;
            } else {
                reader
                    .take(MAX_HTTP_BODY_BYTES as u64)
                    .read_to_end(&mut resp_body)?;
            }
        }

        let content_type = resp_headers.get("content-type").cloned().unwrap_or_default();
        return Ok(RawHttpResponse {
            redacted_url: redact_url(&current_url, secrets),
            url: current_url,
            status,
            content_type,
            headers: resp_headers,
            elapsed_ms: start.elapsed().as_millis() as u64,
            body: resp_body,
        });
    }
    Err(invalid("maximum HTTP redirects (5) exceeded").into())
}

fn read_chunked_body<R: BufRead>(reader: &mut R, max_bytes: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut line_buf = Vec::with_capacity(64);
    for _chunk_idx in 0..65_536 {
        if super::read_bounded_line(reader, &mut line_buf, 4_096)? == 0 {
            break;
        }
        let size_line = String::from_utf8_lossy(&line_buf);
        let trimmed = size_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let hex = trimmed.split(';').next().unwrap_or("");
        let chunk_len = usize::from_str_radix(hex, 16)
            .map_err(|_| invalid(format!("invalid chunked size `{hex}`")))?;
        if chunk_len == 0 {
            break;
        }
        let to_read = chunk_len.min(max_bytes.saturating_sub(out.len()));
        let mut chunk = vec![0u8; to_read];
        reader.read_exact(&mut chunk)?;
        out.extend_from_slice(&chunk);
        if to_read < chunk_len {
            break;
        }
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }
    Ok(out)
}

fn escape_curl_config_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' | '\n' | '\0' => {}
            other => out.push(other),
        }
    }
    out
}

pub fn resolve_redirect_url(
    scheme: &str,
    authority: &str,
    current_path_and_query: &str,
    location: &str,
) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else if let Some(rest) = location.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else {
        let base_path = current_path_and_query
            .split(['?', '#'])
            .next()
            .unwrap_or("/");
        let dir = match base_path.rfind('/') {
            Some(idx) => &base_path[..=idx],
            None => "/",
        };
        format!("{scheme}://{authority}{dir}{location}")
    }
}

/// Executes HTTPS/HTTP via `curl --config -` over stdin (with Rust-controlled redirects so custom sensitive headers never leak across origins).
fn fetch_via_curl(
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout_secs: u64,
    secrets: &[String],
) -> Result<RawHttpResponse> {
    let start = Instant::now();
    let mut current_url = url.to_string();
    let mut cur_method = method.to_string();
    let mut cur_headers: Vec<(String, String)> = headers.to_vec();
    let mut cur_body: Option<&[u8]> = body;
    let mut initial_authority: Option<String> = None;

    let connect_timeout = timeout_secs.clamp(1, 30);
    let max_time = timeout_secs.clamp(1, 60);

    for _hop in 0..=5 {
        let (scheme, rest) = validate_http_url(&current_url)?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if let Some(ref orig) = initial_authority {
            if !orig.eq_ignore_ascii_case(authority) {
                cur_headers = strip_sensitive_redirect_headers(&cur_headers);
            }
        } else {
            initial_authority = Some(authority.to_string());
        }

        let hdr_file = tempfile::NamedTempFile::new()?;
        let body_file = if let Some(b) = cur_body {
            let mut f = tempfile::NamedTempFile::new()?;
            f.write_all(b)?;
            f.flush()?;
            Some(f)
        } else {
            None
        };

        let mut cfg = String::new();
        cfg.push_str(&format!(
            "url = \"{}\"\n",
            escape_curl_config_value(&current_url)
        ));
        cfg.push_str(&format!(
            "request = \"{}\"\n",
            escape_curl_config_value(&cur_method)
        ));
        if cur_method == "HEAD" {
            cfg.push_str("head\n");
        }
        let has_ua = cur_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));
        if !has_ua {
            cfg.push_str(&format!("user-agent = \"{DEFAULT_USER_AGENT}\"\n"));
        }
        for (k, v) in &cur_headers {
            cfg.push_str(&format!(
                "header = \"{}: {}\"\n",
                escape_curl_config_value(k),
                escape_curl_config_value(v)
            ));
        }
        if let Some(ref bf) = body_file {
            cfg.push_str(&format!(
                "data-binary = \"@{}\"\n",
                escape_curl_config_value(&bf.path().to_string_lossy())
            ));
        }

        let mut child = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--compressed",
                "--proto",
                "=http,https",
                "--connect-timeout",
                &connect_timeout.to_string(),
                "--max-time",
                &max_time.to_string(),
                "--max-filesize",
                &MAX_HTTP_BODY_BYTES.to_string(),
                "--dump-header",
            ])
            .arg(hdr_file.path())
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| invalid(format!("failed to invoke `curl`: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(cfg.as_bytes());
        }

        let output = child.wait_with_output()?;
        let raw_hdr_text = std::fs::read_to_string(hdr_file.path()).unwrap_or_default();
        let (status, resp_headers) = parse_last_http_header_block(&raw_hdr_text);

        if !output.status.success() {
            let stderr = scrub_secrets(&String::from_utf8_lossy(&output.stderr), secrets);
            let code_info = output
                .status
                .code()
                .map(|c| format!(" (curl exit {c})"))
                .unwrap_or_default();
            let status_prefix = if status > 0 {
                format!("HTTP {status} transfer aborted{code_info}")
            } else {
                format!("HTTP transport error{code_info}")
            };
            return Err(invalid(format!(
                "{status_prefix} for {}: {}",
                redact_url(&current_url, secrets),
                stderr.trim()
            ))
            .into());
        }

        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp_headers.get("location")
        {
            if (301..=303).contains(&status) && cur_method == "POST" {
                cur_method = "GET".to_string();
                cur_body = None;
            }
            let cur_pq = rest
                .find(['/', '?'])
                .map(|idx| &rest[idx..])
                .unwrap_or("/");
            current_url = resolve_redirect_url(scheme, authority, cur_pq, loc);
            continue;
        }

        let mut resp_body = output.stdout;
        if resp_body.len() > MAX_HTTP_BODY_BYTES {
            resp_body.truncate(MAX_HTTP_BODY_BYTES);
        }
        let content_type = resp_headers.get("content-type").cloned().unwrap_or_default();
        return Ok(RawHttpResponse {
            redacted_url: redact_url(&current_url, secrets),
            url: current_url,
            status,
            content_type,
            headers: resp_headers,
            elapsed_ms: start.elapsed().as_millis() as u64,
            body: resp_body,
        });
    }
    Err(invalid("maximum HTTP redirects (5) exceeded").into())
}

fn parse_last_http_header_block(raw: &str) -> (u16, BTreeMap<String, String>) {
    let mut last_status = 0u16;
    let mut current_headers = BTreeMap::new();
    for line in raw.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.starts_with("HTTP/") {
            if let Some(code) = trimmed
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
            {
                last_status = code;
                current_headers.clear();
            }
        } else if let Some((k, v)) = trimmed.split_once(':') {
            current_headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    (last_status, current_headers)
}

pub fn fetch_single_url(
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout_secs: u64,
    secrets: &[String],
) -> Result<RawHttpResponse> {
    validate_http_url(url)?;
    let upper = method.trim().to_ascii_uppercase();
    if !matches!(upper.as_str(), "GET" | "HEAD" | "POST") {
        return Err(invalid(format!(
            "unsupported HTTP method `{upper}` (expected GET, HEAD, or POST)"
        ))
        .into());
    }
    if is_loopback_http(url) {
        fetch_via_rust_tcp(url, &upper, headers, body, timeout_secs, secrets)
    } else {
        fetch_via_curl(url, &upper, headers, body, timeout_secs, secrets)
    }
}

fn summarize_xml(text: &str) -> Option<XmlSummary> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if !trimmed.starts_with('<') {
        return None;
    }
    // Skip `<?xml ...?>` or `<!DOCTYPE ...>` declarations.
    let mut rest = trimmed;
    while rest.starts_with("<?") || rest.starts_with("<!") {
        if let Some(end) = rest.find('>') {
            rest = rest[end + 1..].trim_start();
        } else {
            break;
        }
    }
    let open = rest.strip_prefix('<')?;
    let root_tag: String = open
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(*c, '_' | '-' | ':'))
        .collect();
    if root_tag.is_empty() || root_tag.eq_ignore_ascii_case("html") {
        return None;
    }

    let mut status_fields = BTreeMap::new();
    for tag in [
        "resultCode",
        "resultMsg",
        "returnReasonCode",
        "returnAuthMsg",
        "errMsg",
        "CODE",
        "MESSAGE",
        "code",
        "message",
    ] {
        if let Some(val) = extract_xml_leaf(trimmed, tag)
            && !val.is_empty()
        {
            status_fields.insert(tag.to_string(), truncate_utf8_chars(&val, 100));
        }
    }

    let mut item_tags = 0usize;
    for marker in ["<item>", "<item ", "<row>", "<row ", "<featureMember", "<entry>"] {
        item_tags += trimmed.matches(marker).count();
    }

    Some(XmlSummary {
        root_tag,
        status_fields,
        item_tags,
    })
}

fn extract_xml_leaf(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    let inner = xml[start..end]
        .trim()
        .trim_start_matches("<![CDATA[")
        .trim_end_matches("]]>")
        .trim();
    Some(inner.to_string())
}

fn extract_html_title(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let gt = start + text[start..].find('>')? + 1;
    let end = gt + lower[gt..].find("</title>")?;
    let title: String = text[gt..end].split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| truncate_utf8_chars(&title, 100))
}

struct EnvBindings {
    query_pairs: Vec<(String, String)>,
    header_pairs: Vec<(String, String)>,
    secrets: Vec<String>,
}

fn resolve_env_bindings(
    env_queries: &[String],
    env_headers: &[String],
) -> Result<EnvBindings> {
    let mut query_pairs = Vec::new();
    let mut header_pairs = Vec::new();
    let mut secrets = Vec::new();

    for spec in env_queries {
        let Some((param, env_raw)) = spec.split_once('=') else {
            return Err(invalid(format!("--env-query expects PARAM=ENV_VAR (got `{spec}`)")).into());
        };
        let param = param.trim();
        let (verbatim, env_name) = if let Some(stripped) = env_raw.trim().strip_prefix('@') {
            (true, stripped.trim())
        } else {
            (false, env_raw.trim())
        };
        if param.is_empty() || env_name.is_empty() {
            return Err(invalid(format!("invalid --env-query `{spec}`")).into());
        }
        let val = std::env::var(env_name)
            .map_err(|_| invalid(format!("environment variable `{env_name}` is not set")))?;
        if val.is_empty() {
            return Err(invalid(format!("environment variable `{env_name}` is empty")).into());
        }
        secrets.push(val.clone());
        let encoded = if verbatim || looks_already_percent_encoded(&val) {
            val
        } else {
            percent_encode_component(&val)
        };
        query_pairs.push((param.to_string(), encoded));
    }

    for spec in env_headers {
        let Some((hdr_name, rhs)) = spec.split_once('=') else {
            return Err(
                invalid(format!("--env-header expects HEADER=ENV_VAR (got `{spec}`)")).into(),
            );
        };
        let (prefix, env_name) = if let Some((scheme, env_n)) = rhs.trim().split_once(':') {
            (format!("{} ", scheme.trim()), env_n.trim())
        } else {
            (String::new(), rhs.trim())
        };
        let val = std::env::var(env_name)
            .map_err(|_| invalid(format!("environment variable `{env_name}` is not set")))?;
        if val.is_empty() {
            return Err(invalid(format!("environment variable `{env_name}` is empty")).into());
        }
        secrets.push(val.clone());
        let (clean_name, clean_val) =
            validate_header_line(&format!("{}: {prefix}{val}", hdr_name.trim()))?;
        header_pairs.push((clean_name, clean_val));
    }

    Ok(EnvBindings {
        query_pairs,
        header_pairs,
        secrets,
    })
}

fn looks_already_percent_encoded(val: &str) -> bool {
    let bytes = val.as_bytes();
    let mut saw_pct = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            saw_pct = true;
            i += 3;
        } else {
            i += 1;
        }
    }
    saw_pct
}

fn append_query_params(url: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        return url.to_string();
    }
    let (base, frag) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    let joined: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={v}", percent_encode_component(k)))
        .collect();
    match frag {
        Some(f) => format!("{base}{sep}{}#{f}", joined.join("&")),
        None => format!("{base}{sep}{}", joined.join("&")),
    }
}

fn summarize_response(
    label: String,
    resp: RawHttpResponse,
    filters: &QueryFilters,
    secrets: &[String],
) -> HttpEndpointSummary {
    let ok = (200..300).contains(&resp.status);
    let ct_lower = resp.content_type.to_ascii_lowercase();

    // 1. Check raster/SVG image payload (tiles, icons, static maps)
    if (ct_lower.starts_with("image/") || image_probe::looks_like_image_magic(&resp.body))
        && let Ok(img) = image_probe::inspect_image_bytes(&resp.redacted_url, &resp.body, None, true)
    {
        return HttpEndpointSummary {
            label,
            url: resp.redacted_url,
            ok,
            status: resp.status,
            elapsed_ms: resp.elapsed_ms,
            bytes: resp.body.len(),
            content_type: resp.content_type,
            kind: "image",
            error: None,
            json: None,
            image: Some(img),
            xml: None,
            text_preview: None,
        };
    }

    let text = scrub_secrets(&String::from_utf8_lossy(&resp.body), secrets);

    // 2. Try JSON / GeoJSON / STAC
    if let Ok(parsed) = parse_json_tolerant(&text)
        && let Ok(mut jsum) = summarize_parsed_value(&resp.redacted_url, "json", &parsed, filters)
    {
        for s in &mut jsum.samples {
            if let Ok(raw_s) = serde_json::to_string(s) {
                let scrubbed = scrub_secrets(&raw_s, secrets);
                if let Ok(re_val) = serde_json::from_str::<Value>(&scrubbed) {
                    *s = re_val;
                }
            }
        }
        return HttpEndpointSummary {
            label,
            url: resp.redacted_url,
            ok,
            status: resp.status,
            elapsed_ms: resp.elapsed_ms,
            bytes: resp.body.len(),
            content_type: resp.content_type,
            kind: "json",
            error: None,
            json: Some(jsum),
            image: None,
            xml: None,
            text_preview: None,
        };
    }

    // 3. Try XML (e.g. OpenAPI service/auth errors, WFS/WMS Capabilities)
    if let Some(xml_sum) = summarize_xml(&text) {
        return HttpEndpointSummary {
            label,
            url: resp.redacted_url,
            ok,
            status: resp.status,
            elapsed_ms: resp.elapsed_ms,
            bytes: resp.body.len(),
            content_type: resp.content_type,
            kind: "xml",
            error: None,
            json: None,
            image: None,
            xml: Some(xml_sum),
            text_preview: None,
        };
    }

    // 4. HTML or plain text fallback
    let preview = if let Some(title) = extract_html_title(&text) {
        format!("title=\"{title}\"")
    } else {
        let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        truncate_utf8_chars(&collapsed, 160)
    };

    HttpEndpointSummary {
        label,
        url: resp.redacted_url,
        ok,
        status: resp.status,
        elapsed_ms: resp.elapsed_ms,
        bytes: resp.body.len(),
        content_type: resp.content_type,
        kind: if resp.body.is_empty() { "empty" } else { "text" },
        error: None,
        json: None,
        image: None,
        xml: None,
        text_preview: (!preview.is_empty()).then_some(preview),
    }
}

fn render_http_batch_text(results: &[HttpEndpointSummary], total_ms: u64) -> String {
    let ok_count = results.iter().filter(|r| r.ok).count();
    let mut lines = vec![format!(
        "http_probe endpoints={} ok={}/{} wall_ms={total_ms}",
        results.len(),
        ok_count,
        results.len()
    )];
    let per_sample_chars = if results.len() > 4 { 240 } else { 480 };

    for r in results {
        if let Some(ref err) = r.error {
            lines.push(format!("[{}] ERR ({}ms): {err}", r.label, r.elapsed_ms));
            continue;
        }
        let mut head = format!(
            "[{}] HTTP {} · {}ms · {}B · {}",
            r.label, r.status, r.elapsed_ms, r.bytes, r.kind
        );
        if let Some(ref j) = r.json {
            if let Some(tc) = j.detected_total_count {
                head.push_str(&format!(" · total={tc}"));
            }
            if let Some(ic) = j.item_count {
                head.push_str(&format!(" · items={ic}"));
            }
            if let Some(mc) = j.matched_count {
                head.push_str(&format!(" · matched={mc}"));
            }
            if let Some(ref st) = j.api_status {
                head.push_str(&format!(" · api=\"{st}\""));
            }
            lines.push(head);
            if !j.item_fields.is_empty() {
                let fs: Vec<String> = j
                    .item_fields
                    .iter()
                    .take(14)
                    .map(|f| format!("{}:{}", f.name, f.kind))
                    .collect();
                lines.push(format!("  fields: {}", fs.join(", ")));
            }
            if !j.matched_ids.is_empty() {
                lines.push(format!("  matched_ids: {:?}", j.matched_ids));
            }
            if !j.counts_by.is_empty() {
                let cs: Vec<String> = j
                    .counts_by
                    .iter()
                    .take(10)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                lines.push(format!("  counts_by: {}", cs.join(", ")));
            }
            if let Some(first) = j.samples.first()
                && let Ok(s) = serde_json::to_string(first)
            {
                lines.push(format!(
                    "  sample: {}",
                    truncate_utf8_chars(&s, per_sample_chars)
                ));
            }
        } else if let Some(ref img) = r.image {
            head.push_str(&format!(
                " · {} {}x{} {}",
                img.format, img.width, img.height, img.color_space
            ));
            lines.push(head);
        } else if let Some(ref xml) = r.xml {
            head.push_str(&format!(" · <{}> items={}", xml.root_tag, xml.item_tags));
            if !xml.status_fields.is_empty() {
                let sf: Vec<String> = xml
                    .status_fields
                    .iter()
                    .map(|(k, v)| format!("{k}=\"{v}\""))
                    .collect();
                head.push_str(&format!(" · {}", sf.join(" ")));
            }
            lines.push(head);
        } else {
            if let Some(ref p) = r.text_preview {
                head.push_str(&format!(" · {p}"));
            }
            lines.push(head);
        }
    }
    clamp_utf8_bytes(&lines.join("\n"), INLINE_BUDGET)
}

pub fn run(args: &HttpProbeArgs) -> Result<String> {
    let batch_start = Instant::now();
    let method = args.method.trim().to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "HEAD" | "POST") {
        return Err(invalid(format!(
            "unsupported HTTP method `{method}` (expected GET, HEAD, or POST)"
        ))
        .into());
    }

    let mut headers = Vec::new();
    let mut secrets = Vec::new();
    for raw_h in &args.headers {
        let (k, v) = validate_header_line(raw_h)?;
        if super::is_sensitive_key_name(&k) && v.len() >= 4 {
            secrets.push(v.clone());
        }
        headers.push((k, v));
    }

    let env_bindings = resolve_env_bindings(&args.env_queries, &args.env_headers)?;
    headers.extend(env_bindings.header_pairs);
    secrets.extend(env_bindings.secrets);

    let expanded = expand_url_templates(&args.urls, &args.vars)?;
    let targets: Vec<(String, String)> = expanded
        .into_iter()
        .map(|(label, u)| {
            validate_http_url(&u)?;
            let clean_label = scrub_secrets(&redact_url(&label, &secrets), &secrets);
            Ok((clean_label, append_query_params(&u, &env_bindings.query_pairs)))
        })
        .collect::<Result<Vec<_>>>()?;

    let bbox = args.bbox.as_deref().map(BBox::parse).transpose()?;
    let predicate = args
        .where_expr
        .as_deref()
        .map(Predicate::parse)
        .transpose()?;
    let filters = QueryFilters {
        path: args.path.clone(),
        bbox,
        predicate,
        count_by: args.count_by.clone(),
        max_samples: args.samples.min(20),
    };

    let worker_count = args.jobs.clamp(1, 16).min(targets.len().max(1));
    let timeout = args.timeout.clamp(1, 60);
    let body_bytes = args.body.as_deref().map(str::as_bytes);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut slots: Vec<Option<HttpEndpointSummary>> = vec![None; targets.len()];
    let results_mutex = std::sync::Mutex::new(&mut slots);

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let idx = next_idx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if idx >= targets.len() {
                        break;
                    }
                    let (ref label, ref target_url) = targets[idx];
                    let item_start = Instant::now();
                    let summary = match fetch_single_url(
                        target_url,
                        &method,
                        &headers,
                        body_bytes,
                        timeout,
                        &secrets,
                    ) {
                        Ok(resp) => summarize_response(label.clone(), resp, &filters, &secrets),
                        Err(e) => HttpEndpointSummary {
                            label: label.clone(),
                            url: redact_url(target_url, &secrets),
                            ok: false,
                            status: 0,
                            elapsed_ms: item_start.elapsed().as_millis() as u64,
                            bytes: 0,
                            content_type: String::new(),
                            kind: "error",
                            error: Some(scrub_secrets(&e.to_string(), &secrets)),
                            json: None,
                            image: None,
                            xml: None,
                            text_preview: None,
                        },
                    };
                    if let Ok(mut guard) = results_mutex.lock() {
                        guard[idx] = Some(summary);
                    }
                }
            });
        }
    });

    let ordered: Vec<HttpEndpointSummary> = slots.into_iter().flatten().collect();
    let wall_ms = batch_start.elapsed().as_millis() as u64;

    if args.json {
        let ok_count = ordered.iter().filter(|r| r.ok).count();
        clamp_json_budget(json!({
            "endpoints": ordered.len(),
            "ok_count": ok_count,
            "wall_ms": wall_ms,
            "results": ordered,
        }))
    } else {
        Ok(render_http_batch_text(&ordered, wall_ms))
    }
}

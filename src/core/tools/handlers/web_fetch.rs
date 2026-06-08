//! Web fetch tool for sned CLI.
//!
//! Fetches web pages via HTTP and converts HTML to readable text.
//! Includes SSRF protection and URL validation.

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;
use futures::StreamExt;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use url::Url;

/// Maximum redirect count to prevent redirect loops and open redirect attacks
const MAX_REDIRECTS: u32 = 5;

/// Default maximum response body size (1MB) to prevent memory exhaustion
/// Configurable via SNED_MAX_FETCH_RESPONSE_SIZE env var.
fn max_response_size() -> usize {
    use std::sync::OnceLock;
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        let max_allowed = 100 * 1024 * 1024;
        std::env::var("SNED_MAX_FETCH_RESPONSE_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&v| v <= max_allowed)
            .unwrap_or(1_048_576)
    })
}

/// Default fetch timeout (30s), configurable via SNED_FETCH_TIMEOUT_SECS env var.
fn fetch_timeout() -> std::time::Duration {
    use std::sync::OnceLock;
    static TIMEOUT: OnceLock<std::time::Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        let secs = std::env::var("SNED_FETCH_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        std::time::Duration::from_secs(secs)
    })
}

/// Consume a chunked response body into a `Vec<u8>` with a hard memory
/// cap. Returns `(bytes, truncated, content_length)`.
///
/// `truncated` is set when either the `Content-Length` header declared a
/// body larger than `max_size` (so the caller knows the body was
/// abbreviated even if the stream finished without hitting the cap), or
/// the byte stream itself crossed the cap mid-read. At most `max_size`
/// bytes are retained regardless of the actual body size — this is the
/// fix for the response-body DoS where `response.bytes().await` would
/// allocate the full body before truncating.
async fn read_response_capped(
    response: reqwest::Response,
    max_size: usize,
) -> Result<(Vec<u8>, bool, Option<u64>), reqwest::Error> {
    let content_length = response.content_length();
    let stream = response
        .bytes_stream()
        .map(|r| r.map(|b| b.to_vec()));
    let (buf, truncated) = collect_capped(content_length, stream, max_size).await?;
    Ok((buf, truncated, content_length))
}

/// Inner loop of `read_response_capped`, decoupled from `reqwest::Response`
/// so it can be unit-tested with a synthetic `futures::stream::iter`.
async fn collect_capped<E, S>(
    content_length: Option<u64>,
    stream: S,
    max_size: usize,
) -> Result<(Vec<u8>, bool), E>
where
    E: std::fmt::Display,
    S: futures::Stream<Item = Result<Vec<u8>, E>> + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len() + chunk.len() > max_size {
            let take = max_size - buf.len();
            buf.extend_from_slice(&chunk[..take]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    if !truncated
        && let Some(len) = content_length
        && (len as usize) > max_size
    {
        truncated = true;
    }
    Ok((buf, truncated))
}

/// Web fetch handler for fetching URLs and returning page content as text.
#[derive(Debug, Clone, Default)]
pub struct WebFetchHandler;

impl WebFetchHandler {
    /// Validate URL to prevent SSRF attacks.
    ///
    /// Blocks:
    /// - Non-HTTP schemes (file://, gopher://, dict://, etc.)
    /// - Private IP ranges (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16)
    /// - Loopback (127.0.0.0/8, ::1)
    /// - Link-local (169.254.0.0/16, fe80::/10)
    /// - Cloud metadata endpoints (169.254.169.254)
    /// - User info in URL (user:pass@host)
    fn validate_url(url_str: &str) -> Result<Url, ToolError> {
        let url = Url::parse(url_str)
            .map_err(|e| ToolError::InvalidInput(format!("Invalid URL format: {}", e)))?;

        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(ToolError::InvalidInput(format!(
                "URL scheme '{}' is not allowed. Only http:// and https:// are permitted.",
                url.scheme()
            )));
        }

        // Block URLs with user info to prevent credential leakage via URL
        if url.username().is_empty() && url.password().is_some() {
            return Err(ToolError::InvalidInput(
                "URLs with passwords are not allowed".to_string(),
            ));
        }
        if !url.username().is_empty() {
            return Err(ToolError::InvalidInput(
                "URLs with embedded credentials are not allowed".to_string(),
            ));
        }

        let host = url
            .host_str()
            .ok_or_else(|| ToolError::InvalidInput("URL must have a valid hostname".to_string()))?;

        // For IPv6, host_str() includes brackets, so we need to strip them
        let ip_str = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            Self::validate_ip(&ip)?;
        } else if host == "metadata.google.internal"
            || host == "169.254.169.254"
            || host.ends_with(".compute.internal")
            || host.ends_with(".internal")
        {
            return Err(ToolError::InvalidInput(
                "Access to internal cloud metadata endpoints is not allowed".to_string(),
            ));
        }

        Ok(url)
    }

    /// Validate an IP address to prevent SSRF attacks.
    fn validate_ip(ip: &IpAddr) -> Result<(), ToolError> {
        match ip {
            IpAddr::V4(ipv4) => {
                // is_private covers 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                if ipv4.is_private() || ipv4.is_loopback() || ipv4.is_link_local() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to private IP address {} is not allowed",
                        ip
                    )));
                }
                // CGNAT/shared address space 100.64.0.0/10 (covers 100.64-100.127)
                let octets = ipv4.octets();
                if octets[0] == 100 && (octets[1] & 0xC0) == 0x40 {
                    return Err(ToolError::InvalidInput(
                        "Access to shared address space (CGNAT 100.64.0.0/10) is not allowed"
                            .to_string(),
                    ));
                }
                if octets == [169, 254, 169, 254] {
                    return Err(ToolError::InvalidInput(
                        "Access to cloud metadata endpoint 169.254.169.254 is not allowed"
                            .to_string(),
                    ));
                }
                if ipv4.octets() == [0, 0, 0, 0] {
                    return Err(ToolError::InvalidInput(
                        "Access to 0.0.0.0 is not allowed".to_string(),
                    ));
                }
            }
            IpAddr::V6(ipv6) => {
                if ipv6.is_unspecified() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to unspecified address {} is not allowed",
                        ip
                    )));
                }
                if ipv6.is_loopback() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to loopback address {} is not allowed",
                        ip
                    )));
                }
                if ipv6.is_unicast_link_local() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to link-local address {} is not allowed",
                        ip
                    )));
                }
                // fc00::/7 — unique local addresses (IPv6 equivalent of private IPv4)
                if ipv6.is_unique_local() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to unique local address {} is not allowed",
                        ip
                    )));
                }
                // IPv4-mapped IPv6 (::ffff:x.x.x.x) — kernel routes to the embedded IPv4
                if let Some(mapped) = ipv6.to_ipv4_mapped() {
                    return Self::validate_ip(&IpAddr::V4(mapped));
                }
            }
        }
        Ok(())
    }

    /// Resolve hostname and validate all resolved IPs against SSRF rules.
    ///
    /// Returns the first validated socket address for connection pinning.
    /// This prevents DNS rebinding attacks where a domain resolves to a
    /// private IP after URL validation passes.
    fn resolve_and_validate(url: &Url) -> Result<SocketAddr, ToolError> {
        let host = url
            .host_str()
            .ok_or_else(|| ToolError::InvalidInput("URL must have a valid hostname".to_string()))?;

        let port = url
            .port()
            .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });

        // For bare IP addresses, validate directly without DNS resolution
        let ip_str = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            Self::validate_ip(&ip)?;
            return Ok(SocketAddr::new(ip, port));
        }

        // Resolve hostname — this is the actual DNS lookup that an attacker
        // could race via rebinding. We validate every resolved IP.
        let addrs: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to resolve hostname '{}': {}", host, e))
            })?
            .collect();

        if addrs.is_empty() {
            return Err(ToolError::ExecutionFailed(format!(
                "DNS resolution for '{}' returned no addresses",
                host
            )));
        }

        let mut first_valid = None;
        for addr in &addrs {
            Self::validate_ip(&addr.ip())?;
            if first_valid.is_none() {
                first_valid = Some(*addr);
            }
        }

        // SAFETY: addrs is non-empty and we validated all entries, so first_valid is Some
        Ok(first_valid.expect("at least one address must exist"))
    }

    /// Fetch a URL and convert HTML to plain text.
    async fn fetch_url(&self, url: &str) -> Result<String, ToolError> {
        let validated_url = Self::validate_url(url)?;

        // Resolve DNS and validate all IPs before connecting — prevents DNS rebinding
        let pinned_addr = Self::resolve_and_validate(&validated_url)?;

        let host = validated_url
            .host_str()
            .ok_or_else(|| ToolError::InvalidInput("URL must have a valid hostname".to_string()))?
            .to_string();

        // Disable automatic redirects — we handle them manually to validate DNS
        // at each hop (reqwest's redirect policy can't do async DNS resolution)
        let mut builder = reqwest::Client::builder()
            .timeout(fetch_timeout())
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .redirect(reqwest::redirect::Policy::none());

        // Pin DNS resolution to the validated address — reqwest will use this
        // instead of performing its own DNS lookup, preventing rebinding
        builder = builder.resolve(&host, pinned_addr);

        let client = builder.build().map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to create HTTP client: {}", e))
        })?;

        // Manual redirect loop with DNS validation at each hop
        let mut current_url = validated_url;
        let mut current_client = client;
        let mut redirect_count: u32 = 0;

        loop {
            let response = current_client
                .get(current_url.clone())
                .send()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("Failed to fetch URL: {}", e)))?;

            let status = response.status();
            if status.is_redirection() {
                if redirect_count >= MAX_REDIRECTS {
                    return Err(ToolError::ExecutionFailed("Too many redirects".to_string()));
                }

                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        ToolError::ExecutionFailed(
                            "Redirect response missing Location header".to_string(),
                        )
                    })?;

                // Resolve relative redirects against current URL
                let redirect_url = Url::parse(location)
                    .or_else(|_| current_url.join(location))
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "Invalid redirect URL '{}': {}",
                            location, e
                        ))
                    })?;

                // Validate the redirect URL and resolve its DNS — prevents
                // rebinding on redirect targets
                let validated_redirect = Self::validate_url(redirect_url.as_str())?;
                let redirect_addr = Self::resolve_and_validate(&validated_redirect)?;

                // Build a new client pinned to the redirect target's resolved IP
                let redirect_host = validated_redirect
                    .host_str()
                    .ok_or_else(|| {
                        ToolError::InvalidInput("Redirect URL must have a hostname".to_string())
                    })?
                    .to_string();

                current_client = reqwest::Client::builder()
                    .timeout(fetch_timeout())
                    .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve(&redirect_host, redirect_addr)
                    .build()
                    .map_err(|e| ToolError::ExecutionFailed(format!("Failed to create HTTP client: {}", e)))?;

                current_url = validated_redirect;
                redirect_count += 1;
                continue;
            }

            if !status.is_success() {
                return Err(ToolError::ExecutionFailed(format!(
                    "HTTP error {} when fetching {}",
                    status, url
                )));
            }

            let max_size = max_response_size();
            let (bytes, truncated, content_length) = read_response_capped(response, max_size)
                .await
                .map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to read response body: {}", e))
                })?;

            let html = String::from_utf8_lossy(&bytes).to_string();
            let text = html_to_text(&html);

            return if truncated {
                let full_size_note = match content_length {
                    Some(len) => format!(", full size was {} bytes", len),
                    None => ", Content-Length not declared".to_string(),
                };
                Ok(format!(
                    "Successfully fetched {} (response truncated at {} bytes{}). Content:\n\n{}",
                    url, max_size, full_size_note, text
                ))
            } else {
                Ok(format!(
                    "Successfully fetched {}. Content:\n\n{}",
                    url, text
                ))
            };
        }
    }

    pub async fn execute(
        &self,
        _state: &mut TaskState,
        params: serde_json::Value,
    ) -> Result<String, ToolError> {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'url' parameter".to_string()))?;
        self.fetch_url(url).await
    }
}

#[async_trait]
impl ToolHandler for WebFetchHandler {
    async fn execute(
        &self,
        _ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        // Don't acquire state lock - web_fetch doesn't use state and holding the lock
        // across HTTP requests blocks Ctrl+C cancellation handler
        Self::execute(self, &mut TaskState::default(), params)
            .await
            .map(serde_json::Value::String)
    }

    fn description(&self, params: &serde_json::Value) -> String {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        format!("[web_fetch for '{}'", url)
    }
}

/// Convert HTML to plain text by stripping tags.
///
/// This is a simplified conversion that removes common HTML tags and
/// replaces block-level tags with newlines.
fn html_to_text(html: &str) -> String {
    // Strip null bytes to prevent bypass attempts (scri\0pt, etc.)
    let cleaned: String = html.chars().filter(|c| *c != '\0').collect();

    let mut text = String::new();
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut prev_was_space = true; // Start true to trim leading whitespace

    let chars: Vec<char> = cleaned.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if ch == '<' {
            let rest: String = chars[i + 1..].iter().take(20).collect();
            let tag_start = rest.to_lowercase();

            if tag_start.starts_with("script") {
                in_script = true;
            } else if tag_start.starts_with("style") {
                in_style = true;
            } else if tag_start.starts_with("/script") {
                in_script = false;
            } else if tag_start.starts_with("/style") {
                in_style = false;
            }

            in_tag = true;
            i += 1;
            continue;
        }

        if in_script || in_style {
            if ch == '>' {
                let rest: String = chars[i + 1..].iter().take(10).collect();
                if rest.to_lowercase().starts_with("</script") {
                    in_script = false;
                } else if rest.to_lowercase().starts_with("</style") {
                    in_style = false;
                }
            }
            i += 1;
            continue;
        }

        if ch == '>' {
            in_tag = false;
            i += 1;
            continue;
        }

        if in_tag {
            i += 1;
            continue;
        }

        if ch.is_whitespace() {
            if !prev_was_space {
                text.push(' ');
                prev_was_space = true;
            }
        } else {
            text.push(ch);
            prev_was_space = false;
        }

        i += 1;
    }

    let text = text.trim();

    // Limit to reasonable size for model context
    const MAX_LENGTH: usize = 50_000;
    if text.len() > MAX_LENGTH {
        // Use floor_char_boundary to avoid splitting multi-byte UTF-8 characters
        let safe_end = text.floor_char_boundary(MAX_LENGTH);
        format!(
            "{}\n\n[Content truncated. Total length: {} characters]",
            &text[..safe_end],
            text.len()
        )
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_to_text_basic() {
        let html = "<html><body><p>Hello world</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn test_html_to_text_removes_script() {
        let html = "<p>Hello</p><script>alert('xss')</script><p>World</p>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn test_html_to_text_strips_null_bytes() {
        // Null bytes should be stripped to prevent bypass attempts
        let html = "<p>Hello</p><scri\u{0000}pt>alert('xss')</scri\u{0000}pt><p>World</p>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("alert"));
        // Verify null bytes are not in output
        assert!(!text.contains('\0'));
    }

    #[test]
    fn test_html_to_text_removes_style() {
        let html = "<p>Hello</p><style>body{color:red}</style><p>World</p>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("color"));
    }

    #[test]
    fn test_html_to_text_collapses_whitespace() {
        let html = "<p>Hello   \n\n   World</p>";
        let text = html_to_text(html);
        assert_eq!(text, "Hello World");
    }

    #[test]
    fn test_html_to_text_truncates_long_content() {
        let html = format!("<p>{}a</p>", "x".repeat(100_000));
        let text = html_to_text(&html);
        assert!(text.contains("[Content truncated"));
        assert!(text.len() < 60_000);
    }

    #[test]
    fn test_validate_url_blocks_non_http_schemes() {
        assert!(WebFetchHandler::validate_url("file:///etc/passwd").is_err());
        assert!(WebFetchHandler::validate_url("gopher://evil.com").is_err());
        assert!(WebFetchHandler::validate_url("dict://evil.com:1111").is_err());
        assert!(WebFetchHandler::validate_url("ftp://files.com/file.txt").is_err());
    }

    #[test]
    fn test_validate_url_allows_http_https() {
        assert!(WebFetchHandler::validate_url("http://example.com").is_ok());
        assert!(WebFetchHandler::validate_url("https://example.com").is_ok());
        assert!(WebFetchHandler::validate_url("https://example.com/path?query=1").is_ok());
    }

    #[test]
    fn test_validate_url_blocks_private_ips() {
        assert!(WebFetchHandler::validate_url("http://10.0.0.1").is_err());
        assert!(WebFetchHandler::validate_url("http://172.16.0.1").is_err());
        assert!(WebFetchHandler::validate_url("http://192.168.1.1").is_err());
        assert!(WebFetchHandler::validate_url("http://127.0.0.1").is_err());
        assert!(WebFetchHandler::validate_url("http://169.254.169.254").is_err());
        assert!(WebFetchHandler::validate_url("http://0.0.0.0").is_err());
    }

    #[test]
    fn test_validate_url_blocks_credentials() {
        assert!(WebFetchHandler::validate_url("http://user:pass@example.com").is_err());
        assert!(WebFetchHandler::validate_url("http://user@example.com").is_err());
    }

    #[test]
    fn test_validate_url_blocks_cloud_metadata() {
        assert!(WebFetchHandler::validate_url("http://metadata.google.internal").is_err());
        assert!(WebFetchHandler::validate_url("http://169.254.169.254").is_err());
    }

    #[test]
    fn test_validate_ip_v6_loopback() {
        assert!(WebFetchHandler::validate_url("http://[::1]").is_err());
    }

    #[test]
    fn test_validate_ip_v6_unique_local() {
        // fc00::/7 unique local addresses must be blocked
        assert!(WebFetchHandler::validate_url("http://[fc00::1]").is_err());
        assert!(WebFetchHandler::validate_url("http://[fd00::1]").is_err());
        assert!(WebFetchHandler::validate_url("http://[fd12:3456:789a::1]").is_err());
    }

    #[test]
    fn test_validate_ip_v4_mapped_v6() {
        // IPv4-mapped IPv6 must be blocked when the embedded IPv4 is private
        assert!(WebFetchHandler::validate_url("http://[::ffff:127.0.0.1]").is_err());
        assert!(WebFetchHandler::validate_url("http://[::ffff:10.0.0.1]").is_err());
        assert!(WebFetchHandler::validate_url("http://[::ffff:192.168.1.1]").is_err());
        assert!(WebFetchHandler::validate_url("http://[::ffff:172.16.0.1]").is_err());
        assert!(WebFetchHandler::validate_url("http://[::ffff:169.254.169.254]").is_err());
    }

    fn chunk_stream(chunks: Vec<Vec<u8>>) -> impl futures::Stream<Item = Result<Vec<u8>, String>> {
        futures::stream::iter(chunks.into_iter().map(Ok::<_, String>))
    }

    #[tokio::test]
    async fn test_collect_capped_under_limit() {
        let s = chunk_stream(vec![b"hello ".to_vec(), b"world".to_vec()]);
        let (buf, truncated) = collect_capped(None, s, 1024).await.unwrap();
        assert_eq!(buf, b"hello world".to_vec());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn test_collect_capped_mid_stream_truncates() {
        // 8-byte cap, 11 bytes total across three chunks. The third
        // chunk crosses the cap and must be cut at the boundary.
        let s = chunk_stream(vec![
            b"hello ".to_vec(),
            b"world".to_vec(),
            b"extra".to_vec(),
        ]);
        let (buf, truncated) = collect_capped(None, s, 8).await.unwrap();
        assert_eq!(buf, b"hello wo".to_vec());
        assert_eq!(buf.len(), 8);
        assert!(truncated);
    }

    #[tokio::test]
    async fn test_collect_capped_content_length_over_limit() {
        // Body fits in the cap, but Content-Length declares a larger
        // body. The caller should still know the response was truncated.
        let s = chunk_stream(vec![b"small".to_vec()]);
        let (buf, truncated) = collect_capped(Some(1_000_000), s, 1024).await.unwrap();
        assert_eq!(buf, b"small".to_vec());
        assert!(truncated);
    }

    #[tokio::test]
    async fn test_collect_capped_empty_stream() {
        let s = chunk_stream(vec![]);
        let (buf, truncated) = collect_capped(None, s, 1024).await.unwrap();
        assert!(buf.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn test_collect_capped_exact_limit_not_truncated() {
        // Body size equals cap exactly: no truncation from stream,
        // and no Content-Length, so we report the full body as read.
        let s = chunk_stream(vec![b"abcdefgh".to_vec()]);
        let (buf, truncated) = collect_capped(None, s, 8).await.unwrap();
        assert_eq!(buf.len(), 8);
        assert!(!truncated);
    }
}

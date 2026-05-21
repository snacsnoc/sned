//! Web fetch tool for sned CLI.
//!
//! Fetches web pages via HTTP and converts HTML to readable text.
//! Includes SSRF protection and URL validation.

use crate::core::agent_loop::TaskState;
use crate::core::tools::{ToolContext, ToolError, ToolHandler};
use async_trait::async_trait;
use std::net::IpAddr;
use url::Url;

/// Maximum redirect count to prevent redirect loops and open redirect attacks
const MAX_REDIRECTS: u32 = 5;

/// Maximum response body size (1MB) to prevent memory exhaustion
const MAX_RESPONSE_SIZE: usize = 1_048_576;

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

        // Only allow HTTP and HTTPS schemes
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(ToolError::InvalidInput(format!(
                "URL scheme '{}' is not allowed. Only http:// and https:// are permitted.",
                url.scheme()
            )));
        }

        // Block URLs with user info (user:pass@host) to prevent credential leakage
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

        // Resolve hostname and check for blocked IPs
        let host = url
            .host_str()
            .ok_or_else(|| ToolError::InvalidInput("URL must have a valid hostname".to_string()))?;

        // Check if host is an IP address and validate it
        // For IPv6, host_str() includes brackets, so we need to strip them
        let ip_str = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            Self::validate_ip(&ip)?;
        } else {
            // For hostnames, block obvious metadata endpoints
            if host == "metadata.google.internal"
                || host == "169.254.169.254"
                || host.ends_with(".compute.internal")
                || host.ends_with(".internal")
            {
                return Err(ToolError::InvalidInput(
                    "Access to internal cloud metadata endpoints is not allowed".to_string(),
                ));
            }
        }

        Ok(url)
    }

    /// Validate an IP address to prevent SSRF attacks.
    fn validate_ip(ip: &IpAddr) -> Result<(), ToolError> {
        match ip {
            IpAddr::V4(ipv4) => {
                // Block private ranges
                if ipv4.is_private() || ipv4.is_loopback() || ipv4.is_link_local() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to private IP address {} is not allowed",
                        ip
                    )));
                }
                // Block cloud metadata endpoints
                if ipv4.octets() == [169, 254, 169, 254] {
                    return Err(ToolError::InvalidInput(
                        "Access to cloud metadata endpoint 169.254.169.254 is not allowed"
                            .to_string(),
                    ));
                }
                // Block 0.0.0.0
                if ipv4.octets() == [0, 0, 0, 0] {
                    return Err(ToolError::InvalidInput(
                        "Access to 0.0.0.0 is not allowed".to_string(),
                    ));
                }
            }
            IpAddr::V6(ipv6) => {
                // Block loopback
                if ipv6.is_loopback() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to loopback address {} is not allowed",
                        ip
                    )));
                }
                // Block link-local (fe80::/10)
                if ipv6.is_unicast_link_local() {
                    return Err(ToolError::InvalidInput(format!(
                        "Access to link-local address {} is not allowed",
                        ip
                    )));
                }
            }
        }
        Ok(())
    }

    /// Fetch a URL and convert HTML to plain text.
    async fn fetch_url(&self, url: &str) -> Result<String, ToolError> {
        // Validate URL to prevent SSRF attacks
        let validated_url = Self::validate_url(url)?;

        let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS as usize {
                attempt.stop()
            } else {
                match Self::validate_url(attempt.url().as_str()) {
                    Ok(_) => attempt.follow(),
                    Err(e) => {
                        tracing::warn!("SSRF: blocking redirect to {}: {}", attempt.url(), e);
                        attempt.stop()
                    }
                }
            }
        });

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .redirect(redirect_policy)
            .build()
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to create HTTP client: {}", e)))?;

        let response = client
            .get(validated_url.clone())
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to fetch URL: {}", e)))?;

        let status = response.status();
        if !status.is_success() {
            return Err(ToolError::ExecutionFailed(format!(
                "HTTP error {} when fetching {}",
                status, url
            )));
        }

        // Read response with size limit to prevent memory exhaustion
        let bytes = response.bytes().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to read response body: {}", e))
        })?;

        if bytes.len() > MAX_RESPONSE_SIZE {
            return Err(ToolError::ExecutionFailed(format!(
                "Response size ({} bytes) exceeds maximum allowed size ({} bytes)",
                bytes.len(),
                MAX_RESPONSE_SIZE
            )));
        }

        let html = String::from_utf8_lossy(&bytes).to_string();

        // Convert HTML to plain text
        let text = html_to_text(&html);

        Ok(format!(
            "Successfully fetched {}. Content:\n\n{}",
            url, text
        ))
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
        ctx: &ToolContext,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let mut state = ctx.state.lock().await;
        Self::execute(self, &mut state, params)
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
            // Look ahead to check for script/style tags
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
                // Check if this closes the script/style tag
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

        // Handle whitespace
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

    // Trim and limit length
    let text = text.trim();

    // Limit to reasonable size for LLM context
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
}

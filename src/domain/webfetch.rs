//! Bounded, SSRF-safe fetching of the pages an operator pointed at.
//!
//! The URLs come from a human, and the server fetches them, so this is a security boundary rather
//! than a convenience: only `http`/`https`, only public addresses, no redirects (a redirect is the
//! cheapest way to reach a private address after a check), a timeout, and a hard byte cap. Every
//! refusal names its reason, so the operator sees which URL was dropped and why.

use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};

/// At most this many URLs are read for one analysis.
pub const MAX_URLS: usize = 5;
/// At most this many bytes are read from one page.
pub const MAX_BYTES: usize = 262_144;
/// At most this many characters reach the prompt after HTML stripping.
pub const MAX_TEXT_CHARS: usize = 8_000;
/// One request may not take longer than this.
pub const TIMEOUT_SECONDS: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Anything that is not http or https, including `file:`, `ftp:` and scheme-less input.
    BlockedScheme(String),
    /// The host does not resolve, or resolves to an address that is not public.
    BlockedHost(String),
    /// A non-success HTTP status.
    Http(u16),
    /// The response exceeded MAX_BYTES.
    TooLarge,
    /// The request did not complete in time.
    Timeout,
    /// Anything else the HTTP client reported.
    Network(String),
}

impl FetchError {
    pub fn reason(&self) -> String {
        match self {
            Self::BlockedScheme(scheme) => format!("blocked: scheme {scheme:?} is not allowed"),
            Self::BlockedHost(host) => {
                format!("blocked: {host} is not a public internet host")
            }
            Self::Http(status) => format!("http {status}"),
            Self::TooLarge => format!("response larger than {MAX_BYTES} bytes"),
            Self::Timeout => format!("no answer within {TIMEOUT_SECONDS}s"),
            Self::Network(message) => format!("network error: {message}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageText {
    pub url: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub url: String,
    /// Present only when the page was read; the reason lives in `error` otherwise.
    pub text: Option<String>,
    pub error: Option<String>,
}

/// Reject an address the process must never reach: loopback, private, link-local (which includes the
/// cloud metadata endpoint), unspecified, multicast and broadcast. IPv4-mapped IPv6 delegates to the
/// IPv4 rules, because `::ffff:127.0.0.1` is still loopback.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_public_v4(mapped);
            }
            let segments = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique local
                || (segments[0] & 0xfe00) == 0xfc00
                // fe80::/10 link local
                || (segments[0] & 0xffc0) == 0xfe80)
        }
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        // 100.64.0.0/10 carrier-grade NAT, 0.0.0.0/8
        || (address.octets()[0] == 100 && (64..128).contains(&address.octets()[1]))
        || address.octets()[0] == 0)
}

/// Check a URL before any request is made: scheme, host and every resolved address.
pub fn check_url(raw: &str) -> Result<(), FetchError> {
    let trimmed = raw.trim();
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => return Err(FetchError::BlockedScheme("none".to_string())),
    };
    if scheme != "http" && scheme != "https" {
        return Err(FetchError::BlockedScheme(scheme));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(FetchError::BlockedHost("empty host".to_string()));
    }
    let host = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    // Drop a port without breaking IPv6 literals written as [::1].
    let host = match host.strip_prefix('[') {
        Some(inner) => match inner.split_once(']') {
            Some((address, _)) => address,
            None => return Err(FetchError::BlockedHost(host.to_string())),
        },
        None => host.split(':').next().unwrap_or(host),
    };
    if host.is_empty() {
        return Err(FetchError::BlockedHost("empty host".to_string()));
    }
    let addresses = (host, 0u16)
        .to_socket_addrs()
        .map_err(|_| FetchError::BlockedHost(host.to_string()))?
        .map(|socket| socket.ip())
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(FetchError::BlockedHost(host.to_string()));
    }
    if let Some(blocked) = addresses.iter().find(|ip| !is_public_ip(**ip)) {
        return Err(FetchError::BlockedHost(format!("{host} ({blocked})")));
    }
    Ok(())
}

/// Strip the parts of an HTML document that never carry meaning for a reader.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(MAX_TEXT_CHARS * 2));
    let mut rest = html;
    while let Some(index) = rest.find('<') {
        out.push_str(&rest[..index]);
        let lowered = rest[index..].to_ascii_lowercase();
        // Skip whole script/style/noscript blocks: their content is not prose.
        let skip = ["<script", "<style", "<noscript"]
            .iter()
            .find(|tag| lowered.starts_with(**tag));
        if let Some(tag) = skip {
            let closing = format!("</{}>", &tag[1..]);
            match lowered.find(&closing) {
                Some(end) => {
                    rest = &rest[index + end + closing.len()..];
                    continue;
                }
                None => break,
            }
        }
        match rest[index..].find('>') {
            Some(end) => rest = &rest[index + end + 1..],
            None => break,
        }
    }
    out.push_str(rest);

    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_TEXT_CHARS).collect()
}

/// Read the pages, one outcome per URL. A failure never hides the other results.
pub async fn read_pages(urls: &[String]) -> Vec<FetchOutcome> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        // A redirect is the shortest path from an allowed host to a blocked one.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("lead-radar/0.1 (project description analysis)")
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return urls
                .iter()
                .map(|url| FetchOutcome {
                    url: url.clone(),
                    text: None,
                    error: Some(FetchError::Network(error.to_string()).reason()),
                })
                .collect()
        }
    };

    let mut outcomes = Vec::new();
    for url in urls.iter().take(MAX_URLS) {
        outcomes.push(read_one(&client, url).await);
    }
    outcomes
}

async fn read_one(client: &reqwest::Client, url: &str) -> FetchOutcome {
    if let Err(error) = check_url(url) {
        return FetchOutcome {
            url: url.to_string(),
            text: None,
            error: Some(error.reason()),
        };
    }
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(error) => {
            let reason = if error.is_timeout() {
                FetchError::Timeout
            } else {
                FetchError::Network(error.without_url().to_string())
            };
            return FetchOutcome {
                url: url.to_string(),
                text: None,
                error: Some(reason.reason()),
            };
        }
    };
    if !response.status().is_success() {
        return FetchOutcome {
            url: url.to_string(),
            text: None,
            error: Some(FetchError::Http(response.status().as_u16()).reason()),
        };
    }

    let mut body = Vec::new();
    let mut response = response;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_BYTES {
                    return FetchOutcome {
                        url: url.to_string(),
                        text: None,
                        error: Some(FetchError::TooLarge.reason()),
                    };
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                return FetchOutcome {
                    url: url.to_string(),
                    text: None,
                    error: Some(FetchError::Network(error.to_string()).reason()),
                }
            }
        }
    }

    let raw = String::from_utf8_lossy(&body);
    let text = html_to_text(&raw);
    if text.trim().is_empty() {
        return FetchOutcome {
            url: url.to_string(),
            text: None,
            error: Some("no readable text on the page".to_string()),
        };
    }
    FetchOutcome {
        url: url.to_string(),
        text: Some(text),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_private_addresses_are_not_public() {
        for address in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.5",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            let parsed: IpAddr = address.parse().expect("ip");
            assert!(!is_public_ip(parsed), "{address} must not be reachable");
        }
        for address in ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"] {
            let parsed: IpAddr = address.parse().expect("ip");
            assert!(is_public_ip(parsed), "{address} is public");
        }
    }

    #[test]
    fn non_http_schemes_and_local_hosts_are_refused() {
        assert!(matches!(
            check_url("file:///etc/passwd"),
            Err(FetchError::BlockedScheme(_))
        ));
        assert!(matches!(
            check_url("ftp://example.com/x"),
            Err(FetchError::BlockedScheme(_))
        ));
        assert!(matches!(
            check_url("example.com/no-scheme"),
            Err(FetchError::BlockedScheme(_))
        ));
        assert!(matches!(
            check_url("http://localhost:8080/"),
            Err(FetchError::BlockedHost(_))
        ));
        assert!(matches!(
            check_url("http://169.254.169.254/latest/meta-data/"),
            Err(FetchError::BlockedHost(_))
        ));
        assert!(matches!(
            check_url("http://127.0.0.1/"),
            Err(FetchError::BlockedHost(_))
        ));
    }

    #[test]
    fn html_is_reduced_to_readable_text() {
        let html = "<html><head><style>body{color:red}</style><script>var x = 1 < 2;</script>\
                    </head><body><h1>Live translation</h1><p>We run 40 person conferences &amp; \
                    workshops.</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Live translation"), "{text}");
        assert!(text.contains("conferences & workshops"), "{text}");
        assert!(!text.contains("var x"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
        assert!(!text.contains('<'), "{text}");
    }

    #[test]
    fn text_is_capped() {
        let html = "<p>".to_string() + &"a".repeat(MAX_TEXT_CHARS * 2);
        assert_eq!(html_to_text(&html).chars().count(), MAX_TEXT_CHARS);
    }
}

use super::*;
use crate::config::PortPolicy;

fn guard() -> NetworkGuard {
    NetworkGuard::new(NetworkPolicy::default())
}

#[test]
fn iana_non_global_ranges_and_embedded_ipv4_are_denied() {
    for raw in [
        "100.64.0.1",
        "198.18.0.1",
        "::ffff:127.0.0.1",
        "64:ff9b::7f00:1",
        "64:ff9b:1::1",
        "100::1",
        "3fff::1",
        "5f00::1",
    ] {
        assert!(guard().validate_ip(raw.parse().unwrap()).is_err(), "{raw}");
    }
    assert!(guard()
        .validate_ip("2606:4700:4700::1111".parse().unwrap())
        .is_ok());
}

#[test]
fn default_exceptions_allow_carved_out_addresses() {
    // 192.0.0.9 sits inside the denied 192.0.0.0/24 but is a documented
    // exception; 192.0.2.1 in the same range is not.
    assert!(guard().validate_ip("192.0.0.9".parse().unwrap()).is_ok());
    assert!(guard().validate_ip("2001:1::1".parse().unwrap()).is_ok());
    assert_eq!(
        guard()
            .validate_ip("192.0.2.1".parse().unwrap())
            .unwrap_err()
            .to_string(),
        "network target denied: address 192.0.2.1 is denied by the CIDR policy"
    );
}

#[test]
fn explicit_allow_and_deny_lists_beat_the_default_policy() {
    let denied = NetworkPolicy {
        denied_cidrs: vec!["93.184.216.0/24".parse().unwrap()],
        ..NetworkPolicy::default()
    };
    assert_eq!(
        NetworkGuard::new(denied)
            .validate_ip("93.184.216.34".parse().unwrap())
            .unwrap_err()
            .to_string(),
        "network target denied: address 93.184.216.34 is denied by the CIDR policy"
    );

    let allowed = NetworkPolicy {
        allowed_cidrs: vec!["10.0.0.0/8".parse().unwrap()],
        ..NetworkPolicy::default()
    };
    assert!(NetworkGuard::new(allowed)
        .validate_ip("10.1.2.3".parse().unwrap())
        .is_ok());

    let permissive = NetworkPolicy {
        deny_non_global: false,
        ..NetworkPolicy::default()
    };
    assert!(NetworkGuard::new(permissive)
        .validate_ip("10.1.2.3".parse().unwrap())
        .is_ok());
}

#[test]
fn validate_url_rejects_non_web_schemes_credentials_and_ports() {
    for (url, ports, expected) in [
        (
            "ftp://example.com/file",
            PortPolicy::Any,
            "invalid URL: only http and https URLs are supported",
        ),
        (
            "http://user:pass@example.com/",
            PortPolicy::Any,
            "invalid URL: embedded URL credentials are not allowed",
        ),
        (
            "http://user@example.com/",
            PortPolicy::Any,
            "invalid URL: embedded URL credentials are not allowed",
        ),
        (
            "http://example.com:8080/",
            PortPolicy::WebOnly,
            "network target denied: TCP port 8080 is denied by policy",
        ),
        (
            "http://example.com:9090/",
            PortPolicy::Explicit(vec![8080]),
            "network target denied: TCP port 9090 is denied by policy",
        ),
    ] {
        let policy = NetworkPolicy {
            allowed_ports: ports,
            ..NetworkPolicy::default()
        };
        let url = Url::parse(url).unwrap();
        assert_eq!(
            validate_url(&url, &policy).unwrap_err().to_string(),
            expected,
            "{url}"
        );
    }
    for (url, ports) in [
        ("https://example.com/page", PortPolicy::WebOnly),
        ("http://example.com/", PortPolicy::WebOnly),
        ("http://example.com:8080/", PortPolicy::Any),
        (
            "http://example.com:8080/",
            PortPolicy::Explicit(vec![8080]),
        ),
    ] {
        let policy = NetworkPolicy {
            allowed_ports: ports,
            ..NetworkPolicy::default()
        };
        let url = Url::parse(url).unwrap();
        assert!(validate_url(&url, &policy).is_ok(), "{url}");
    }
}

#[test]
fn retry_after_accepts_seconds_and_http_dates_only() {
    assert_eq!(
        parse_retry_after("120"),
        Some(Duration::from_secs(120))
    );
    assert_eq!(parse_retry_after(" 42 "), Some(Duration::from_secs(42)));
    assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
        Some(Duration::ZERO)
    );
    assert_eq!(parse_retry_after("soon"), None);
    assert_eq!(parse_retry_after("-1"), None);

    // A future HTTP-date deadline yields the remaining window; the exact
    // value moves with the wall clock, so pin it from below.
    let remaining = parse_retry_after("Mon, 01 Jan 2035 00:00:00 GMT")
        .expect("a future HTTP date yields a delay");
    assert!(
        remaining > Duration::from_secs(86_400 * 365),
        "remaining was {remaining:?}"
    );
}

#[test]
fn origin_key_fills_in_default_ports() {
    assert_eq!(
        origin_key(&Url::parse("https://example.com/a").unwrap()),
        "https://example.com:443"
    );
    assert_eq!(
        origin_key(&Url::parse("http://example.com:8080/").unwrap()),
        "http://example.com:8080"
    );
    assert_eq!(
        origin_key(&Url::parse("http://EXAMPLE.com/").unwrap()),
        "http://example.com:80"
    );
}

#[test]
fn diagnostics_redact_query_values_and_credentials() {
    let url = Url::parse("https://example.test/path?secret=value&x=1").unwrap();
    assert_eq!(
        safe_url(&url),
        "https://example.test/path?secret=REDACTED&x=REDACTED"
    );
    let credentialed = Url::parse("https://user:pass@example.test/path").unwrap();
    assert_eq!(safe_url(&credentialed), "https://example.test/path");
}

#[test]
fn only_actual_redirect_statuses_are_followed() {
    for status in [301, 302, 303, 307, 308] {
        assert!(is_followed_redirect(status), "{status} must be followed");
    }
    for status in [200, 204, 300, 304, 305, 306, 310, 400] {
        assert!(!is_followed_redirect(status), "{status} must be ignored");
    }
}

#[test]
fn decodable_document_families_pass_the_content_type_preflight() {
    for value in [
        "text/html",
        "TEXT/HTML; charset=utf-8",
        "  application/xhtml+xml  ",
        "text/plain; charset=iso-8859-1",
        "text/csv",
        "application/json",
        "application/xml",
        "application/vnd.api+json",
        "image/svg+xml",
    ] {
        assert!(
            is_decodable_content_type(Some(value)),
            "{value} must be decodable"
        );
    }
}

#[test]
fn binary_and_missing_content_types_fail_the_preflight() {
    for value in [
        "application/pdf",
        "image/png",
        "image/jpeg",
        "video/mp4",
        "application/octet-stream",
        "application/zip",
        "font/woff2",
        "",
    ] {
        assert!(
            !is_decodable_content_type(Some(value)),
            "{value} must be rejected"
        );
    }
    assert!(!is_decodable_content_type(None));
}

#[derive(Debug)]
struct Chained {
    message: String,
    next: Option<Box<Chained>>,
}

impl Chained {
    fn chain(messages: &[&str]) -> Self {
        messages
            .iter()
            .rev()
            .fold(None, |next: Option<Box<Chained>>, message| {
                Some(Box::new(Chained {
                    message: (*message).to_string(),
                    next,
                }))
            })
            .map(|boxed| *boxed)
            .expect("a chain has at least one message")
    }
}

impl fmt::Display for Chained {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Chained {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.next
            .as_deref()
            .map(|next| next as &(dyn StdError + 'static))
    }
}

#[test]
fn error_chains_redact_credential_markers_and_truncate() {
    // The walk starts below the top error, so the head message never
    // reaches the diagnostic, and nested causes join with ": ".
    let clean = Chained::chain(&["request failed", "connect failed", "tls aborted"]);
    assert_eq!(
        safe_error_message(&clean),
        "connect failed: tls aborted"
    );
    // Repeated nested messages collapse instead of echoing.
    assert_eq!(safe_error_message(&Chained::chain(&["head", "dup", "dup"])), "dup");
    // No nested cause falls back to a generic message.
    assert_eq!(
        safe_error_message(&Chained::chain(&["lonely"])),
        "request failed"
    );
    // Credential-like markers anywhere in the chain replace the text.
    for marker in ["authorization", "cookie", "token", "api_key", "api-key"] {
        let leaky = Chained::chain(&["outer", &format!("prefix {marker} suffix")]);
        assert_eq!(
            safe_error_message(&leaky),
            "network error contained credential-like data",
            "{marker}"
        );
    }
    // Diagnostics are bounded at 500 characters.
    let long = Chained::chain(&["outer", &"x".repeat(600)]);
    assert_eq!(safe_error_message(&long).chars().count(), 500);
}

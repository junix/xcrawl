use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::thread;

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xcrawl"))
        .args(args)
        .output()
        .expect("xcrawl binary should start")
}

#[test]
fn help_exposes_security_outcome_and_resource_policies_without_build_path() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for flag in [
        "--max-links-to-analyze",
        "--max-links-to-enqueue",
        "--max-links-to-report",
        "--max-origin-in-flight",
        "--redirect-policy",
        "--max-http-requests",
        "--max-total-download-bytes",
        "--max-crawl-duration",
        "--max-report-bytes",
        "--allow-partial",
        "--fail-on-any-error",
        "--format",
    ] {
        assert!(help.contains(flag), "help omitted {flag}");
    }
    assert!(!help.contains(env!("CARGO_MANIFEST_DIR")));
}

#[test]
fn dry_run_reports_the_grouped_effective_policy_without_network() {
    let output = run(&[
        "https://unreachable.invalid/docs/start",
        "--max-pages",
        "7",
        "--max-depth",
        "4",
        "--concurrency",
        "3",
        "--max-origin-in-flight",
        "2",
        "--max-links-to-analyze",
        "30",
        "--max-links-to-enqueue",
        "25",
        "--max-links-to-report",
        "10",
        "--strategy",
        "dfs",
        "--domain-scope",
        "--allow-subdomains",
        "--include-path-prefix",
        "/docs",
        "--exclude-path-prefix",
        "/docs/private",
        "--delay",
        "125ms",
        "--timeout",
        "2m",
        "--max-response-bytes",
        "2MiB",
        "--max-retries",
        "1",
        "--redirect-policy",
        "same-origin",
        "--user-agent",
        "xcrawl-test/1.0",
        "--dry-run",
        "--compact",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["schema_version"], 2);
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["seed_url"], "https://unreachable.invalid/docs/start");
    assert_eq!(plan["config"]["limits"]["max_pages"], 7);
    assert_eq!(plan["config"]["traversal"]["max_depth"], 4);
    assert_eq!(plan["config"]["traversal"]["concurrency"], 3);
    assert_eq!(plan["config"]["traversal"]["max_origin_in_flight"], 2);
    assert_eq!(plan["config"]["traversal"]["max_links_to_analyze"], 30);
    assert_eq!(plan["config"]["traversal"]["max_links_to_enqueue"], 25);
    assert_eq!(plan["config"]["traversal"]["max_links_to_report"], 10);
    assert_eq!(plan["config"]["traversal"]["strategy"], "depth_first");
    assert_eq!(plan["config"]["traversal"]["default_delay_ms"], 125);
    assert_eq!(plan["config"]["scope"]["boundary"], "domain");
    assert_eq!(plan["config"]["scope"]["allow_subdomains"], true);
    assert_eq!(
        plan["config"]["scope"]["include_path_prefixes"],
        serde_json::json!(["/docs"])
    );
    assert_eq!(
        plan["config"]["scope"]["exclude_path_prefixes"],
        serde_json::json!(["/docs/private"])
    );
    assert_eq!(plan["config"]["scope"]["path_match_mode"], "segment_prefix");
    assert_eq!(plan["config"]["scope"]["redirect_policy"], "same_origin");
    assert_eq!(plan["config"]["scope"]["max_redirects"], 5);
    assert_eq!(plan["config"]["robots"]["respect"], true);
    assert_eq!(plan["config"]["retry"]["max_attempts"], 2);
    assert_eq!(plan["config"]["limits"]["max_response_bytes"], 2_097_152);
    // --timeout 2m lands on the per-attempt deadline, not the crawl deadline.
    assert_eq!(plan["config"]["limits"]["max_attempt_duration_ms"], 120_000);
    assert_eq!(plan["config"]["network"]["user_agent"], "xcrawl-test/1.0");
}

#[test]
fn invalid_policy_and_credentialed_seed_fail_without_leaking_secrets() {
    let invalid_policy = run(&["https://example.com", "--concurrency", "0", "--dry-run"]);
    assert_eq!(invalid_policy.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&invalid_policy.stderr);
    assert!(
        stderr.contains(
            "page, concurrency, link, request, byte, origin, frontier, URL, and report limits must be positive"
        ),
        "got: {stderr}"
    );

    let conflict = run(&[
        "https://example.com",
        "--allow-partial",
        "--fail-on-any-error",
        "--dry-run",
    ]);
    assert_eq!(conflict.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(
        stderr.contains("--allow-partial conflicts with --fail-on-any-error"),
        "got: {stderr}"
    );

    let credentialed = run(&["https://user:super-secret@example.com", "--dry-run"]);
    assert_eq!(credentialed.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&credentialed.stderr);
    assert!(stderr.contains("embedded URL credentials are not allowed"));
    assert!(!stderr.contains("super-secret"));
}

#[test]
fn conflicting_scope_flags_fail_as_invalid_policy() {
    let conflict = run(&[
        "https://example.com",
        "--allow-cross-domain",
        "--domain-scope",
        "--dry-run",
    ]);
    assert_eq!(conflict.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(
        stderr.contains("--allow-cross-domain conflicts with --domain-scope"),
        "got: {stderr}"
    );

    let orphan_subdomains =
        run(&["https://example.com", "--allow-subdomains", "--dry-run"]);
    assert_eq!(orphan_subdomains.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&orphan_subdomains.stderr);
    assert!(
        stderr.contains("--allow-subdomains requires --domain-scope"),
        "got: {stderr}"
    );
}

#[test]
fn malformed_flag_values_exit_2_as_usage_errors() {
    for (flag, value, expected_diagnostic) in [
        (
            "--delay",
            "5x",
            "expected a duration ending in ms, s, or m",
        ),
        (
            "--max-response-bytes",
            "12ZiB",
            "invalid byte size: 12ZiB",
        ),
        ("--strategy", "lateral", "invalid value 'lateral'"),
        ("--max-pages", "many", "invalid value 'many'"),
    ] {
        let output = run(&["https://example.com", "--dry-run", flag, value]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{flag} {value} must be a usage error"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected_diagnostic),
            "{flag} {value}: got {stderr}"
        );
    }
}

#[test]
fn unreachable_seed_is_nonzero_and_structured() {
    let output = run(&[
        "http://unreachable.invalid/",
        "--ignore-robots",
        "--max-retries",
        "0",
        "--timeout",
        "1s",
        "--dns-timeout",
        "1s",
        "--format",
        "json",
        "--compact",
    ]);
    assert_eq!(output.status.code(), Some(4));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "seed_failed");
    assert_eq!(report["failures"][0]["url"], "http://unreachable.invalid/");
    assert_eq!(report["failures"][0]["error"]["kind"], "network");
    assert_eq!(report["failures"][0]["error"]["attempts"], 1);
    assert_eq!(report["failures"][0]["error"]["retryable"], true);
    assert_eq!(report["stats"]["http_requests"], 1);
    assert_eq!(report["stats"]["pages_crawled"], 0);
    assert!(report["pages"].as_array().is_some_and(Vec::is_empty));
}

#[test]
fn partial_requires_an_explicit_allow_partial_override() {
    let (url, _server) = partial_site();
    let strict = run(&[
        &url,
        "--ignore-robots",
        "--allow-private-networks",
        "--allow-nonstandard-ports",
        "--max-pages",
        "2",
        "--max-retries",
        "0",
        "--delay",
        "0ms",
        "--format",
        "json",
        "--compact",
    ]);
    assert_eq!(strict.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&strict.stdout).unwrap();
    assert_eq!(report["outcome"], "partial");
    assert_eq!(report["failures"].as_array().map(Vec::len), Some(1));
    assert_eq!(report["failures"][0]["error"]["kind"], "http_status");
    assert_eq!(report["failures"][0]["error"]["status"], 500);

    let (url, _server) = partial_site();
    let allowed = run(&[
        &url,
        "--ignore-robots",
        "--allow-private-networks",
        "--allow-nonstandard-ports",
        "--max-pages",
        "2",
        "--max-retries",
        "0",
        "--delay",
        "0ms",
        "--format",
        "json",
        "--compact",
        "--allow-partial",
    ]);
    assert_eq!(allowed.status.code(), Some(0));
    // The override changes only the exit code: the crawl is still reported
    // as the same partial outcome, not silently upgraded to complete.
    let report: serde_json::Value = serde_json::from_slice(&allowed.stdout).unwrap();
    assert_eq!(report["outcome"], "partial");
    assert_eq!(report["failures"].as_array().map(Vec::len), Some(1));
    assert_eq!(report["failures"][0]["error"]["kind"], "http_status");
}

#[test]
fn jsonl_is_the_default_and_broken_pipe_is_successful_cancellation() {
    let output = run(&["https://example.com", "--dry-run", "--compact"]);
    assert!(output.status.success());

    let mut child = Command::new(env!("CARGO_BIN_EXE_xcrawl"))
        .args([
            "http://unreachable.invalid/",
            "--ignore-robots",
            "--max-retries",
            "0",
            "--timeout",
            "1s",
            "--dns-timeout",
            "1s",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let stderr = child.stderr.take();
    let status = child.wait().unwrap();
    assert!(status.success());
    // Success must come from graceful cancellation, not from an error path
    // that happens to exit zero while diagnosing the failure on stderr.
    let mut diagnostics = String::new();
    if let Some(mut stream) = stderr {
        let _ = stream.read_to_string(&mut diagnostics);
    }
    assert!(!diagnostics.contains("error:"), "{diagnostics}");
}

fn partial_site() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..size]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or("/");
            let (status, body) = if path == "/" {
                (
                    "200 OK",
                    "<article><h1>root</h1><p>root body</p></article><a href='/bad'>bad</a>",
                )
            } else {
                ("500 Internal Server Error", "")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}/"), handle)
}

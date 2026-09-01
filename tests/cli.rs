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
fn the_default_dry_run_plan_pins_the_conservative_defaults() {
    // No policy flags at all: the plan must show the locked-down posture.
    let output = run(&["https://example.com", "--dry-run", "--compact"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Scope starts at the exact seed origin, with robots honored.
    assert_eq!(plan["config"]["scope"]["boundary"], "origin");
    assert_eq!(plan["config"]["scope"]["allow_subdomains"], false);
    assert_eq!(plan["config"]["scope"]["path_match_mode"], "segment_prefix");
    assert_eq!(
        plan["config"]["scope"]["redirect_policy"],
        "within_crawl_scope"
    );
    assert_eq!(plan["config"]["scope"]["allow_https_downgrade"], false);
    assert_eq!(plan["config"]["scope"]["max_redirects"], 5);
    assert_eq!(plan["config"]["robots"]["respect"], true);
    // Traversal is breadth-first with one request per origin at a time.
    assert_eq!(plan["config"]["traversal"]["strategy"], "breadth_first");
    assert_eq!(plan["config"]["traversal"]["max_depth"], 2);
    assert_eq!(plan["config"]["traversal"]["concurrency"], 8);
    assert_eq!(plan["config"]["traversal"]["max_origin_in_flight"], 1);
    assert_eq!(plan["config"]["traversal"]["default_delay_ms"], 250);
    // Retry defaults to three attempts that honor Retry-After.
    assert_eq!(plan["config"]["retry"]["max_attempts"], 3);
    assert_eq!(plan["config"]["retry"]["honor_retry_after"], true);
    // The default network posture denies private ranges and redacts queries.
    assert_eq!(plan["config"]["network"]["deny_non_global"], true);
    assert_eq!(plan["config"]["output"]["redact_query_values"], true);
    assert_eq!(plan["config"]["limits"]["max_pages"], 100);
    assert_eq!(plan["config"]["limits"]["max_http_requests"], 1_000);
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
        (
            "--delay",
            "xs",
            "invalid duration value: xs",
        ),
        (
            "--delay",
            "18446744073709551615m",
            "duration is too large: 18446744073709551615m",
        ),
        (
            "--max-response-bytes",
            "18446744073709551615KiB",
            "byte size is too large: 18446744073709551615KiB",
        ),
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

    // A seed that is not a URL at all is a usage error (2) from the value
    // parser, not an invalid policy (3).
    let output = run(&["notaurl", "--dry-run"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value 'notaurl' for '<URL>'"),
        "got: {stderr}"
    );
}

#[test]
fn byte_size_units_decode_decimal_binary_and_unitless_values() {
    for (flag, value, field, expected) in [
        // "MB" is the decimal unit and must win over the bare "b" suffix.
        (
            "--max-response-bytes",
            "5MB",
            "max_response_bytes",
            5_000_000_u64,
        ),
        (
            "--max-report-bytes",
            "3GB",
            "max_report_bytes",
            3_000_000_000_u64,
        ),
        // A value without a unit is already a byte count.
        (
            "--max-response-bytes",
            "2048",
            "max_response_bytes",
            2_048_u64,
        ),
    ] {
        let output = run(&["https://example.com", "--dry-run", "--compact", flag, value]);
        assert!(
            output.status.success(),
            "{flag} {value}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["config"]["limits"][field], expected, "{flag} {value}");
    }
}

#[test]
fn denied_seed_targets_and_retry_overflow_keep_their_exit_codes_apart() {
    // 255 retries + the initial attempt does not fit u8.
    let overflow = run(&["https://example.com", "--max-retries", "255", "--dry-run"]);
    assert_eq!(overflow.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&overflow.stderr);
    assert!(stderr.contains("max_retries is too large"), "got: {stderr}");

    let non_web = run(&["ftp://example.com/", "--dry-run"]);
    assert_eq!(non_web.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&non_web.stderr);
    assert!(
        stderr.contains("only http and https URLs are supported"),
        "got: {stderr}"
    );

    // A policy-denied TCP port is a network denial (4), not an invalid
    // policy (3), even though the flag parsed cleanly.
    let denied_port = run(&["http://example.com:8080/", "--dry-run"]);
    assert_eq!(denied_port.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&denied_port.stderr);
    assert!(
        stderr.contains("network target denied: TCP port 8080 is denied by policy"),
        "got: {stderr}"
    );
}

#[test]
fn a_complete_crawl_exits_zero_and_pretty_prints_without_compact() {
    let (url, _server) = healthy_site();
    let output = run(&[
        &url,
        "--ignore-robots",
        "--allow-private-networks",
        "--allow-nonstandard-ports",
        "--max-retries",
        "0",
        "--delay",
        "0ms",
        "--format",
        "json",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Without --compact the report is pretty-printed with one field per line;
    // the compact form is asserted in the JSON Lines test.
    assert!(stdout.starts_with("{\n"), "not pretty: {stdout}");
    assert!(stdout.contains("\n  \"outcome\""), "not pretty: {stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["outcome"], "complete");
    assert_eq!(report["stats"]["pages_crawled"], 2);
    assert_eq!(report["pages"].as_array().map(Vec::len), Some(2));
    assert_eq!(report["failures"].as_array().map(Vec::len), Some(0));
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
    let stdout = String::from_utf8(output.stdout).unwrap();
    // --compact keeps the whole plan on a single line.
    assert_eq!(stdout.lines().count(), 1, "{stdout}");

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

#[test]
fn the_default_jsonl_stream_emits_records_in_protocol_order() {
    let (url, _server) = healthy_site();
    // No --format flag: JSON Lines is the default output mode.
    let output = run(&[
        &url,
        "--ignore-robots",
        "--allow-private-networks",
        "--allow-nonstandard-ports",
        "--max-retries",
        "0",
        "--delay",
        "0ms",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is one JSON record"))
        .collect();
    let good = format!("{}good", url);
    // One record per wire event: the seed request, its discovery, the page
    // event and page record, then the same pair for the linked page, then
    // the completion event and the summary. Requests precede the
    // discoveries they produced, and the summary is always last.
    let tags: Vec<&str> = records
        .iter()
        .map(|record| record["record"].as_str().expect("tagged record"))
        .collect();
    assert_eq!(
        tags,
        ["event", "event", "event", "page", "event", "event", "page", "event", "summary"],
        "{stdout}"
    );
    assert_eq!(records[0]["value"]["event"], "request");
    assert_eq!(records[0]["value"]["kind"], "page");
    assert_eq!(records[0]["value"]["attempt"], 1);
    assert_eq!(records[0]["value"]["status"], 200);
    assert_eq!(records[1]["value"]["event"], "discovered");
    assert_eq!(records[1]["value"]["url"], good);
    assert_eq!(records[1]["value"]["depth"], 1);
    assert_eq!(records[2]["value"]["event"], "page");
    assert_eq!(records[3]["value"]["status"], 200);
    assert_eq!(records[3]["value"]["depth"], 0);
    assert_eq!(records[4]["value"]["event"], "request");
    assert_eq!(records[4]["value"]["url"], good);
    assert_eq!(records[6]["value"]["status"], 200);
    assert_eq!(records[6]["value"]["final_url"], good);
    assert_eq!(records[6]["value"]["depth"], 1);
    assert_eq!(records[7]["value"]["event"], "complete");
    assert_eq!(records[7]["value"]["outcome"], "complete");
    assert_eq!(records[8]["value"]["outcome"], "complete");
    assert_eq!(records[8]["value"]["stats"]["pages_crawled"], 2);
    assert!(records[8]["value"].get("termination_reason").is_none());
}

#[test]
fn a_deadline_crawl_exits_one_and_names_the_deadline() {
    let (url, _server) = chain_site();
    // 120ms of pacing per hop cannot finish a six-page chain inside a
    // 300ms crawl deadline, so the deadline is what terminates the crawl.
    let output = run(&[
        &url,
        "--ignore-robots",
        "--allow-private-networks",
        "--allow-nonstandard-ports",
        "--max-retries",
        "0",
        "--delay",
        "120ms",
        "--max-crawl-duration",
        "300ms",
        "--timeout",
        "250ms",
        "--dns-timeout",
        "100ms",
        "--retry-base-delay",
        "40ms",
        "--retry-max-delay",
        "50ms",
        "--max-depth",
        "5",
        "--max-pages",
        "10",
        "--format",
        "json",
        "--compact",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "deadline_exceeded");
    assert_eq!(report["termination_reason"], "crawl deadline exceeded");
    // Deadline termination is not a page failure.
    assert_eq!(report["failures"].as_array().map(Vec::len), Some(0));
    let crawled = report["stats"]["pages_crawled"].as_u64().unwrap();
    assert!(
        (1..6).contains(&crawled),
        "crawled {crawled} of the six-page chain"
    );
    assert_eq!(report["pages"].as_array().map(Vec::len), Some(crawled as usize));
}

#[test]
fn seed_scope_and_url_length_rejections_have_distinct_exit_codes() {
    // A seed outside the configured include prefix is an invalid policy (3)...
    let excluded = run(&[
        "https://example.com/blog",
        "--include-path-prefix",
        "/docs",
        "--dry-run",
    ]);
    assert_eq!(excluded.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&excluded.stderr);
    assert!(
        stderr.contains("seed URL is excluded by the configured scope policy"),
        "got: {stderr}"
    );

    // ...while an over-long seed trips a resource budget, which is fatal (1)
    // even though the URL itself parsed cleanly.
    let long = format!("https://example.com/{}", "a".repeat(8_300));
    let oversized = run(&[&long, "--dry-run"]);
    assert_eq!(oversized.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&oversized.stderr);
    assert!(
        stderr.contains("resource budget exhausted: url_length limit 8192"),
        "got: {stderr}"
    );
}

#[test]
fn policy_flag_aliases_map_to_exact_plan_values() {
    let output = run(&[
        "https://example.com/docs",
        "--dry-run",
        "--compact",
        "--allow-cross-domain",
        "--raw-path-prefix",
        "--allow-https-downgrade",
        "--ignore-robots",
        "--include-query-values",
        "--max-links-per-page",
        "12",
        "--max-robots-delay",
        "30s",
        "--max-robots-bytes",
        "600KiB",
        "--max-robots-redirects",
        "6",
        "--max-redirects",
        "9",
        "--retry-base-delay",
        "50ms",
        "--retry-max-delay",
        "2s",
        "--ignore-retry-after",
        "--max-crawl-duration",
        "9m",
        "--max-unique-origins",
        "3",
        "--max-frontier-entries",
        "40",
        "--max-url-length",
        "1000",
        "--max-http-requests",
        "77",
        "--max-total-download-bytes",
        "10MiB",
        "--max-report-bytes",
        "1MiB",
        "--dns-timeout",
        "3s",
        "--allow-private-networks",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Scope flag aliases select their boundary and matching mode.
    assert_eq!(plan["config"]["scope"]["boundary"], "any");
    assert_eq!(plan["config"]["scope"]["path_match_mode"], "raw_prefix");
    assert_eq!(plan["config"]["scope"]["allow_https_downgrade"], true);
    assert_eq!(plan["config"]["scope"]["max_redirects"], 9);
    // Robots opt-out and floors survive into the plan.
    assert_eq!(plan["config"]["robots"]["respect"], false);
    assert_eq!(plan["config"]["robots"]["max_delay_ms"], 30_000);
    assert_eq!(plan["config"]["robots"]["max_redirects"], 6);
    assert_eq!(plan["config"]["limits"]["max_robots_bytes"], 614_400);
    // The --max-links-per-page alias feeds the enqueue bound.
    assert_eq!(plan["config"]["traversal"]["max_links_to_enqueue"], 12);
    // Retry knobs, including the Retry-After opt-out.
    assert_eq!(plan["config"]["retry"]["base_delay_ms"], 50);
    assert_eq!(plan["config"]["retry"]["max_delay_ms"], 2_000);
    assert_eq!(plan["config"]["retry"]["honor_retry_after"], false);
    // The remaining resource limits round-trip exactly.
    assert_eq!(plan["config"]["limits"]["max_crawl_duration_ms"], 540_000);
    assert_eq!(plan["config"]["limits"]["max_unique_origins"], 3);
    assert_eq!(plan["config"]["limits"]["max_frontier_entries"], 40);
    assert_eq!(plan["config"]["limits"]["max_url_length"], 1_000);
    assert_eq!(plan["config"]["limits"]["max_http_requests"], 77);
    assert_eq!(
        plan["config"]["limits"]["max_total_download_bytes"],
        10_485_760
    );
    assert_eq!(plan["config"]["limits"]["max_report_bytes"], 1_048_576);
    // Network switches and the query-value disclosure opt-in.
    assert_eq!(plan["config"]["network"]["deny_non_global"], false);
    assert_eq!(plan["config"]["network"]["dns_timeout_ms"], 3_000);
    assert_eq!(plan["config"]["output"]["redact_query_values"], false);
}

fn chain_site() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for _ in 0..6 {
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
            let next = match path {
                "/" => Some("/a"),
                "/a" => Some("/b"),
                "/b" => Some("/c"),
                "/c" => Some("/d"),
                "/d" => Some("/e"),
                _ => None,
            };
            let body = match next {
                Some(next) => format!("<article><h1>hop</h1></article><a href='{next}'>next</a>"),
                None => "<article><h1>end</h1><p>chain end</p></article>".to_string(),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}/"), handle)
}

fn healthy_site() -> (String, thread::JoinHandle<()>) {
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
                    "<article><h1>root</h1><p>root body</p></article><a href='/good'>good</a>",
                )
            } else {
                ("200 OK", "<article><h1>good</h1><p>good body</p></article>")
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

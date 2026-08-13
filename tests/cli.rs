use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xcrawl"))
        .args(args)
        .output()
        .expect("xcrawl binary should start")
}

#[test]
fn help_exposes_the_complete_crawl_policy() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for flag in [
        "--max-links-per-page",
        "--allow-cross-domain",
        "--include-path-prefix",
        "--follow-nofollow",
        "--delay",
        "--timeout",
        "--max-download-bytes",
        "--deny-cross-origin-redirects",
        "--user-agent",
        "--dry-run",
        "--compact",
        "--verbose",
    ] {
        assert!(help.contains(flag), "help omitted {flag}");
    }
    assert!(help.contains(env!("CARGO_MANIFEST_DIR")));
}

#[test]
fn dry_run_accepts_and_reports_all_policy_flags_without_network() {
    let output = run(&[
        "https://unreachable.invalid/start",
        "--max-pages",
        "7",
        "--max-depth",
        "4",
        "--concurrency",
        "3",
        "--max-links-per-page",
        "25",
        "--strategy",
        "dfs",
        "--allow-cross-domain",
        "--allow-subdomains",
        "--include-path-prefix",
        "/docs",
        "--include-path-prefix",
        "/news",
        "--exclude-path-prefix",
        "/private",
        "--follow-nofollow",
        "--ignore-robots",
        "--delay",
        "125ms",
        "--timeout",
        "2m",
        "--max-download-bytes",
        "2MiB",
        "--max-redirects",
        "3",
        "--max-retries",
        "1",
        "--deny-cross-origin-redirects",
        "--allow-private-networks",
        "--user-agent",
        "xcrawl-test/1.0",
        "--dry-run",
        "--compact",
        "-vv",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let plan: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let config = &plan["config"];
    assert_eq!(plan["dry_run"], true);
    assert_eq!(config["max_pages"], 7);
    assert_eq!(config["max_depth"], 4);
    assert_eq!(config["concurrency"], 3);
    assert_eq!(config["max_links_per_page"], 25);
    assert_eq!(config["strategy"], "depth_first");
    assert_eq!(config["stay_on_domain"], false);
    assert_eq!(config["allow_subdomains"], true);
    assert_eq!(
        config["include_path_prefixes"],
        serde_json::json!(["/docs", "/news"])
    );
    assert_eq!(
        config["exclude_path_prefixes"],
        serde_json::json!(["/private"])
    );
    assert_eq!(config["follow_nofollow"], true);
    assert_eq!(config["respect_robots"], false);
    assert_eq!(config["default_delay_ms"], 125);
    assert_eq!(config["request_timeout_ms"], 120_000);
    assert_eq!(config["max_download_bytes"], 2_097_152);
    assert_eq!(config["max_redirects"], 3);
    assert_eq!(config["max_retries"], 1);
    assert_eq!(config["allow_cross_origin_redirects"], false);
    assert_eq!(config["allow_private_networks"], true);
    assert_eq!(config["user_agent"], "xcrawl-test/1.0");
}

#[test]
fn invalid_policy_and_invalid_flag_values_fail_explicitly() {
    let invalid_policy = run(&["https://example.com", "--concurrency", "0", "--dry-run"]);
    assert_eq!(invalid_policy.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&invalid_policy.stderr).contains("invalid crawl configuration")
    );

    let invalid_duration = run(&["https://example.com", "--timeout", "soon", "--dry-run"]);
    assert_eq!(invalid_duration.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid_duration.stderr).contains("expected a duration"));

    let invalid_prefix = run(&[
        "https://example.com",
        "--include-path-prefix",
        "docs",
        "--dry-run",
    ]);
    assert_eq!(invalid_prefix.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&invalid_prefix.stderr).contains("must begin with '/'"));
}

#[test]
fn dry_run_rejects_credentialed_seed_without_echoing_the_secret() {
    let output = run(&["https://user:super-secret@example.com", "--dry-run"]);
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("embedded URL credentials are not allowed"));
    assert!(!stderr.contains("super-secret"));
}

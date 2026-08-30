use std::time::Duration;

use xcrawl::{CrawlConfig, PortPolicy};

fn expect_invalid(name: &str, mutate: impl Fn(&mut CrawlConfig), expected: &str) {
    let mut config = CrawlConfig::default();
    mutate(&mut config);
    let error = config
        .validate()
        .expect_err(name)
        .to_string();
    assert_eq!(error, expected, "{name}");
}

#[test]
fn the_default_config_is_valid() {
    CrawlConfig::default().validate().expect("defaults validate");
}

#[test]
fn enforcement_gated_rules_relax_when_robots_is_disabled() {
    let mut config = CrawlConfig::default();
    config.robots.respect = false;
    config.limits.max_robots_bytes = 1;
    config.robots.max_redirects = 0;
    config
        .validate()
        .expect("robots-gated rules are not checked when enforcement is off");
}

#[test]
fn every_validation_branch_reports_its_exact_diagnostic() {
    for (name, mutate, expected) in [
        (
            "max_pages is positive",
            &(|config: &mut CrawlConfig| config.limits.max_pages = 0) as &dyn Fn(&mut CrawlConfig),
            "invalid crawl configuration: page, concurrency, link, request, byte, origin, \
             frontier, URL, and report limits must be positive",
        ),
        (
            "frontier capacity is positive",
            &|config: &mut CrawlConfig| config.limits.max_frontier_entries = 0,
            "invalid crawl configuration: page, concurrency, link, request, byte, origin, \
             frontier, URL, and report limits must be positive",
        ),
        (
            "report bound is positive",
            &|config: &mut CrawlConfig| config.traversal.max_links_to_report = 0,
            "invalid crawl configuration: page, concurrency, link, request, byte, origin, \
             frontier, URL, and report limits must be positive",
        ),
        (
            "robots body budget keeps the RFC floor",
            &|config: &mut CrawlConfig| config.limits.max_robots_bytes = 500 * 1024 - 1,
            "invalid crawl configuration: max_robots_bytes must be at least 500 KiB when \
             robots enforcement is enabled",
        ),
        (
            "DNS timeout is positive",
            &|config: &mut CrawlConfig| config.network.dns_timeout = Duration::ZERO,
            "invalid crawl configuration: crawl, attempt, DNS, and retry time limits must be \
             positive",
        ),
        (
            "retry ceiling is positive",
            &|config: &mut CrawlConfig| config.retry.max_delay = Duration::ZERO,
            "invalid crawl configuration: crawl, attempt, DNS, and retry time limits must be \
             positive",
        ),
        (
            "robots delay stays at sixty seconds",
            &|config: &mut CrawlConfig| config.robots.max_delay = Duration::from_secs(61),
            "invalid crawl configuration: robots max_delay must not exceed 60 seconds",
        ),
        (
            "crawl deadline stays under a day",
            &|config: &mut CrawlConfig| {
                config.limits.max_crawl_duration = Duration::from_secs(24 * 60 * 60 + 1);
            },
            "invalid crawl configuration: max_crawl_duration must not exceed 24 hours",
        ),
        (
            "attempt duration fits the crawl deadline",
            &|config: &mut CrawlConfig| config.limits.max_crawl_duration = Duration::from_secs(1),
            "invalid crawl configuration: attempt, DNS, throttle, and retry durations must fit \
             inside the crawl deadline",
        ),
        (
            "retry attempts have a lower bound",
            &|config: &mut CrawlConfig| config.retry.max_attempts = 0,
            "invalid crawl configuration: retry max_attempts must be between 1 and 10",
        ),
        (
            "retry attempts have an upper bound",
            &|config: &mut CrawlConfig| config.retry.max_attempts = 11,
            "invalid crawl configuration: retry max_attempts must be between 1 and 10",
        ),
        (
            "retry base delay fits the ceiling",
            &|config: &mut CrawlConfig| config.retry.base_delay = Duration::from_secs(6),
            "invalid crawl configuration: retry base_delay must not exceed retry max_delay",
        ),
        (
            "user agent is not blank",
            &|config: &mut CrawlConfig| config.network.user_agent = "   ".to_string(),
            "invalid crawl configuration: user_agent must not be empty",
        ),
        (
            "include prefixes are rooted",
            &|config: &mut CrawlConfig| {
                config.scope.include_path_prefixes = vec!["docs".to_string()];
            },
            "invalid crawl configuration: include and exclude path prefixes must begin with '/'",
        ),
        (
            "exclude prefixes are rooted",
            &|config: &mut CrawlConfig| {
                config.scope.exclude_path_prefixes = vec!["private".to_string()];
            },
            "invalid crawl configuration: include and exclude path prefixes must begin with '/'",
        ),
        (
            "robots redirects keep the RFC floor",
            &|config: &mut CrawlConfig| config.robots.max_redirects = 4,
            "invalid crawl configuration: robots max_redirects must be at least 5 when \
             enforcement is enabled",
        ),
        (
            "explicit port policy is non-empty",
            &|config: &mut CrawlConfig| config.network.allowed_ports = PortPolicy::Explicit(vec![]),
            "invalid crawl configuration: explicit port policy must contain at least one port",
        ),
    ] {
        expect_invalid(name, mutate, expected);
    }
}

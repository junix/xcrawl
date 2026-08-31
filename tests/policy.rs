use std::fmt::Write as FmtWrite;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;
use xcrawl::{
    CrawlConfig, CrawlOutcome, Crawler, PortPolicy, RedirectPolicy, RobotsDecision, ScopeBoundary,
};

#[derive(Clone)]
struct Reply {
    status: &'static str,
    headers: Vec<(String, String)>,
    body: String,
    delay: Duration,
    close_without_response: bool,
}

impl Reply {
    fn ok(body: impl Into<String>) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
            body: body.into(),
            delay: Duration::ZERO,
            close_without_response: false,
        }
    }

    fn status(status: &'static str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
            delay: Duration::ZERO,
            close_without_response: false,
        }
    }
}

async fn serve(
    handler: Arc<dyn Fn(&str) -> Reply + Send + Sync>,
) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut request = vec![0_u8; 16 * 1024];
                let size = stream.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..size]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or("/");
                let reply = handler(path);
                if reply.close_without_response {
                    return;
                }
                if !reply.delay.is_zero() {
                    tokio::time::sleep(reply.delay).await;
                }
                let mut headers = reply.headers;
                headers.push(("Content-Length".into(), reply.body.len().to_string()));
                headers.push(("Connection".into(), "close".into()));
                let headers =
                    headers
                        .into_iter()
                        .fold(String::new(), |mut output, (name, value)| {
                            write!(output, "{name}: {value}\r\n").unwrap();
                            output
                        });
                let response =
                    format!("HTTP/1.1 {}\r\n{}\r\n{}", reply.status, headers, reply.body);
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (Url::parse(&format!("http://{address}/")).unwrap(), handle)
}

fn local_config() -> CrawlConfig {
    let mut config = CrawlConfig::default();
    config.network.deny_non_global = false;
    config.network.allowed_ports = PortPolicy::Any;
    config.traversal.default_delay = Duration::ZERO;
    config.traversal.max_origin_in_flight = 4;
    config.retry.base_delay = Duration::from_millis(1);
    config.retry.max_delay = Duration::from_millis(10);
    config.limits.max_crawl_duration = Duration::from_secs(5);
    config.limits.max_attempt_duration = Duration::from_secs(2);
    config.network.dns_timeout = Duration::from_secs(1);
    config.output.redact_query_values = false;
    config
}

#[tokio::test]
async fn every_redirect_target_gets_its_own_robots_decision() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let robots_hits = Arc::new(AtomicUsize::new(0));
    let (target, target_server) = serve({
        let target_hits = Arc::clone(&target_hits);
        let robots_hits = Arc::clone(&robots_hits);
        Arc::new(move |path| match path {
            "/robots.txt" => {
                robots_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("User-agent: *\nDisallow: /denied\n")
            }
            "/denied" => {
                target_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("<article>must not be fetched</article>")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let redirect = target.join("/denied").unwrap();
    let (seed, seed_server) = serve(Arc::new(move |path| match path {
        "/robots.txt" => Reply::ok("User-agent: *\nAllow: /\n"),
        "/" => Reply {
            status: "302 Found",
            headers: vec![("Location".into(), redirect.to_string())],
            body: String::new(),
            delay: Duration::ZERO,
            close_without_response: false,
        },
        _ => Reply::status("404 Not Found"),
    }))
    .await;

    let mut config = local_config();
    config.scope.boundary = ScopeBoundary::Any;
    config.scope.redirect_policy = RedirectPolicy::Any;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    seed_server.abort();
    target_server.abort();

    assert_eq!(report.outcome, CrawlOutcome::SeedFailed);
    assert_eq!(robots_hits.load(Ordering::SeqCst), 1);
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
    // The seed failure is the robots denial of the redirect target itself.
    assert_eq!(
        report.failures[0].error.kind,
        xcrawl::FailureKind::RobotsDenied
    );
    assert!(report.failures[0].error.message.contains("/denied"));
}

#[tokio::test]
async fn robots_5xx_and_network_failure_fail_closed() {
    for network_failure in [false, true] {
        let page_hits = Arc::new(AtomicUsize::new(0));
        let (seed, server) = serve({
            let page_hits = Arc::clone(&page_hits);
            Arc::new(move |path| match path {
                "/robots.txt" if network_failure => Reply {
                    close_without_response: true,
                    ..Reply::status("500 Internal Server Error")
                },
                "/robots.txt" => Reply::status("503 Service Unavailable"),
                "/" => {
                    page_hits.fetch_add(1, Ordering::SeqCst);
                    Reply::ok("<article>must not be fetched</article>")
                }
                _ => Reply::status("404 Not Found"),
            })
        })
        .await;
        let mut config = local_config();
        config.retry.max_attempts = 1;
        let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
        server.abort();
        assert_eq!(report.outcome, CrawlOutcome::SeedFailed);
        assert_eq!(page_hits.load(Ordering::SeqCst), 0);
        assert!(report.events.iter().any(|event| matches!(
            event,
            xcrawl::CrawlEvent::Robots {
                decision: RobotsDecision::UnreachableDisallow,
                ..
            }
        )));
    }
}

#[tokio::test]
async fn robots_404_is_unavailable_and_allows_the_page() {
    let page_hits = Arc::new(AtomicUsize::new(0));
    let (seed, server) = serve({
        let page_hits = Arc::clone(&page_hits);
        Arc::new(move |path| match path {
            "/" => {
                page_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("<article><h1>allowed</h1><p>robots unavailable</p></article>")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let mut config = local_config();
    config.retry.max_attempts = 1;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::Complete);
    assert_eq!(page_hits.load(Ordering::SeqCst), 1);
    assert!(report.events.iter().any(|event| matches!(
        event,
        xcrawl::CrawlEvent::Robots {
            decision: RobotsDecision::UnavailableAllow,
            ..
        }
    )));
}

#[tokio::test]
async fn redirect_into_an_excluded_path_is_rejected_before_request() {
    let private_hits = Arc::new(AtomicUsize::new(0));
    let (base, server) = serve({
        let private_hits = Arc::clone(&private_hits);
        Arc::new(move |path| match path {
            "/public" => Reply {
                status: "302 Found",
                headers: vec![("Location".into(), "/private/secret".into())],
                body: String::new(),
                delay: Duration::ZERO,
                close_without_response: false,
            },
            "/private/secret" => {
                private_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("must not be fetched")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config
        .scope
        .exclude_path_prefixes
        .push("/private".to_string());
    let seed = base.join("/public").unwrap();
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::SeedFailed);
    assert_eq!(private_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        report.failures[0].error.kind,
        xcrawl::FailureKind::RedirectDenied
    );
}

#[tokio::test]
async fn retries_reacquire_the_origin_delay() {
    let times = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (seed, server) = serve({
        let times = Arc::clone(&times);
        let attempts = Arc::clone(&attempts);
        Arc::new(move |_| {
            times.lock().unwrap().push(Instant::now());
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                Reply::status("503 Service Unavailable")
            } else {
                Reply::ok("<article><h1>ok</h1><p>retry success</p></article>")
            }
        })
    })
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.retry.max_attempts = 3;
    config.traversal.default_delay = Duration::from_millis(40);
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::Complete);
    let times = times.lock().unwrap();
    assert_eq!(times.len(), 3);
    for pair in times.windows(2) {
        let interval = pair[1].duration_since(pair[0]);
        assert!(
            interval >= Duration::from_millis(25),
            "retry interval was {interval:?}"
        );
    }
}

#[tokio::test]
async fn continuous_scheduler_refills_a_slot_before_a_slow_peer_finishes() {
    let slow_started = Arc::new(Mutex::new(None::<Instant>));
    let new_started = Arc::new(Mutex::new(None::<Instant>));
    let (seed, server) = serve({
        let slow_started = Arc::clone(&slow_started);
        let new_started = Arc::clone(&new_started);
        Arc::new(move |path| match path {
            "/" => Reply::ok("<a href='/slow'>slow</a><a href='/fast'>fast</a>"),
            "/slow" => {
                *slow_started.lock().unwrap() = Some(Instant::now());
                Reply {
                    delay: Duration::from_millis(300),
                    ..Reply::ok("<article>slow page</article>")
                }
            }
            "/fast" => Reply::ok("<a href='/new'>new</a>"),
            "/new" => {
                *new_started.lock().unwrap() = Some(Instant::now());
                Reply::ok("<article>new page</article>")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.traversal.concurrency = 2;
    config.traversal.max_depth = 2;
    config.limits.max_pages = 4;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::Complete);
    let slow = slow_started.lock().unwrap().unwrap();
    let new = new_started.lock().unwrap().unwrap();
    assert!(new.duration_since(slow) < Duration::from_millis(250));
}

#[tokio::test]
async fn request_budget_counts_redirect_hops() {
    let (seed, server) = serve(Arc::new(move |path| match path {
        "/" => Reply {
            status: "302 Found",
            headers: vec![("Location".into(), "/final".into())],
            body: String::new(),
            delay: Duration::ZERO,
            close_without_response: false,
        },
        "/final" => Reply::ok("<article>final</article>"),
        _ => Reply::status("404 Not Found"),
    }))
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.limits.max_http_requests = 1;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::SeedFailed);
    assert_eq!(report.stats.http_requests, 1);
    assert_eq!(
        report.failures[0].error.kind,
        xcrawl::FailureKind::ResourceBudget
    );
    assert_eq!(
        report.failures[0].error.message,
        "resource budget exhausted: http_requests limit 1"
    );
}

#[tokio::test]
async fn total_download_budget_is_enforced_while_streaming() {
    let body = format!("<article>{}</article>", "x".repeat(100));
    let (seed, server) = serve(Arc::new(move |_| Reply::ok(body.clone()))).await;
    let mut config = local_config();
    config.robots.respect = false;
    config.limits.max_total_download_bytes = 32;
    config.limits.max_response_bytes = 1_024;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::SeedFailed);
    assert_eq!(
        report.failures[0].error.kind,
        xcrawl::FailureKind::ResourceBudget
    );
    assert_eq!(
        report.failures[0].error.message,
        "resource budget exhausted: download_bytes limit 32"
    );
    // The failing reserve clamps the counter to the limit, so the crawl
    // reports exactly the budget, never a byte more or less.
    assert_eq!(report.stats.downloaded_bytes, 32);
}

#[tokio::test]
async fn reported_links_are_bounded_and_total_is_preserved() {
    let links = (0..50).fold(String::new(), |mut output, index| {
        write!(output, "<a href='/p{index}'>p{index}</a>").unwrap();
        output
    });
    let (seed, server) = serve(Arc::new(move |_| Reply::ok(links.clone()))).await;
    let mut config = local_config();
    config.robots.respect = false;
    config.limits.max_pages = 1;
    config.traversal.max_links_to_analyze = 20;
    config.traversal.max_links_to_enqueue = 2;
    config.traversal.max_links_to_report = 5;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.pages[0].links_discovered, 50);
    assert_eq!(report.pages[0].links.len(), 5);
    assert!(report.pages[0].links_truncated);
    // The report keeps the first links in document order, not an arbitrary
    // five of the fifty discovered.
    let reported: Vec<&str> = report.pages[0]
        .links
        .iter()
        .map(|link| link.url.path())
        .collect();
    assert_eq!(reported, ["/p0", "/p1", "/p2", "/p3", "/p4"]);
}

#[tokio::test]
async fn undecodable_page_body_is_not_downloaded() {
    let body = format!("%PDF-1.7\n{}", "x".repeat(64 * 1024));
    let (seed, server) = serve(Arc::new(move |_| Reply {
        status: "200 OK",
        headers: vec![("Content-Type".into(), "application/pdf".into())],
        body: body.clone(),
        delay: Duration::ZERO,
        close_without_response: false,
    }))
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();

    assert_eq!(report.outcome, CrawlOutcome::Complete);
    assert_eq!(report.pages.len(), 1);
    let page = &report.pages[0];
    assert_eq!(page.status, 200);
    assert_eq!(page.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(page.body_bytes, 0);
    assert_eq!(
        page.article_error.as_ref().expect("unsupported body").kind,
        "unsupported"
    );
    // Zero download-side proof: 64 KiB of PDF bytes stayed on the wire.
    assert_eq!(report.stats.downloaded_bytes, 0);
}

#[tokio::test]
async fn missing_content_type_page_body_is_not_downloaded() {
    let (seed, server) = serve(Arc::new(move |_| Reply {
        status: "200 OK",
        headers: Vec::new(),
        body: "<html><body>mystery bytes</body></html>".to_string(),
        delay: Duration::ZERO,
        close_without_response: false,
    }))
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();

    assert_eq!(report.outcome, CrawlOutcome::Complete);
    let page = &report.pages[0];
    assert_eq!(page.status, 200);
    assert!(page.content_type.is_none());
    assert_eq!(page.body_bytes, 0);
    assert_eq!(
        page.article_error.as_ref().expect("unsupported body").kind,
        "unsupported"
    );
    assert_eq!(report.stats.downloaded_bytes, 0);
}

#[tokio::test]
async fn decodable_text_page_body_still_downloads() {
    const BODY: &str = "plain text notes that must still be downloaded";
    let (seed, server) = serve(Arc::new(move |_| Reply {
        status: "200 OK",
        headers: vec![("Content-Type".into(), "text/plain".into())],
        body: BODY.to_string(),
        delay: Duration::ZERO,
        close_without_response: false,
    }))
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();

    assert_eq!(report.outcome, CrawlOutcome::Complete);
    let page = &report.pages[0];
    assert_eq!(page.content_type.as_deref(), Some("text/plain"));
    assert_eq!(page.body_bytes, BODY.len());
    assert_eq!(report.stats.downloaded_bytes, page.body_bytes);
}

#[tokio::test]
async fn cross_origin_redirect_denial_names_target_and_action() {
    // Second origin so the redirect genuinely changes origin.
    let (other, other_server) = serve(Arc::new(|_| Reply::ok("<article>other</article>"))).await;
    let (seed, server) = serve(Arc::new({
        let other = other.clone();
        move |_| Reply {
            status: "302 Found",
            headers: vec![("Location".into(), other.to_string())],
            body: String::new(),
            delay: Duration::ZERO,
            close_without_response: false,
        }
    }))
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.scope.redirect_policy = RedirectPolicy::SameOrigin;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    other_server.abort();
    assert_eq!(report.outcome, CrawlOutcome::SeedFailed);
    let failure = &report.failures[0].error;
    assert_eq!(failure.kind, xcrawl::FailureKind::RedirectDenied);
    // The denial must be observable: which origin was targeted and what to
    // do about it (dsh web-fetch-http provider.ts:80-92 wording). The
    // `CrawlError::RedirectDenied` Display prefixes "redirect denied: ".
    let other_origin = other.as_str().trim_end_matches('/');
    assert!(
        failure.message.contains(&format!(
            "cross-origin redirect to {other_origin} is not followed automatically; retry against that URL directly"
        )),
        "got: {}",
        failure.message
    );
}

#[tokio::test]
async fn redirect_budget_message_names_the_hop_limit() {
    let (seed, server) = serve(Arc::new(|path| Reply {
        status: "302 Found",
        headers: vec![(
            "Location".into(),
            if path == "/a" { "/b" } else { "/a" }.into(),
        )],
        body: String::new(),
        delay: Duration::ZERO,
        close_without_response: false,
    }))
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.scope.max_redirects = 2;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::SeedFailed);
    let failure = &report.failures[0].error;
    assert_eq!(failure.kind, xcrawl::FailureKind::RedirectBudget);
    // The concrete hop cap, not an anonymous "budget exhausted"
    // (dsh web-fetch-http provider.ts:64-67 wording).
    assert_eq!(failure.message, "exceeded the maximum of 2 redirects");
}

#[tokio::test]
async fn nofollow_links_are_filtered_unless_following_is_enabled() {
    for follow in [false, true] {
        let followed_hits = Arc::new(AtomicUsize::new(0));
        let skipped_hits = Arc::new(AtomicUsize::new(0));
        let (seed, server) = serve({
            let followed_hits = Arc::clone(&followed_hits);
            let skipped_hits = Arc::clone(&skipped_hits);
            Arc::new(move |path| match path {
                "/" => Reply::ok(
                    "<a href='/followed'>plain</a><a href='/skipped' rel='nofollow'>skipped</a>",
                ),
                "/followed" => {
                    followed_hits.fetch_add(1, Ordering::SeqCst);
                    Reply::ok("<article>followed page</article>")
                }
                "/skipped" => {
                    skipped_hits.fetch_add(1, Ordering::SeqCst);
                    Reply::ok("<article>must not be fetched</article>")
                }
                _ => Reply::status("404 Not Found"),
            })
        })
        .await;
        let mut config = local_config();
        config.robots.respect = false;
        config.traversal.follow_nofollow = follow;
        config.limits.max_pages = 5;
        let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
        server.abort();

        assert_eq!(report.outcome, CrawlOutcome::Complete, "follow={follow}");
        assert_eq!(followed_hits.load(Ordering::SeqCst), 1, "follow={follow}");
        if follow {
            assert_eq!(skipped_hits.load(Ordering::SeqCst), 1);
            assert_eq!(report.stats.pages_crawled, 3);
            // Nothing was filtered: both links were scheduled.
            assert_eq!(report.stats.urls_filtered, 0);
        } else {
            // The rel=nofollow link is skipped at enqueue time: no request,
            // no page record, and exactly one filtered URL.
            assert_eq!(skipped_hits.load(Ordering::SeqCst), 0);
            assert_eq!(report.stats.pages_crawled, 2);
            assert_eq!(report.stats.urls_filtered, 1);
        }
    }
}

#[tokio::test]
async fn zero_depth_crawls_only_the_seed_but_still_reports_its_links() {
    let deeper_hits = Arc::new(AtomicUsize::new(0));
    let (seed, server) = serve({
        let deeper_hits = Arc::clone(&deeper_hits);
        Arc::new(move |path| match path {
            "/" => Reply::ok("<a href='/deeper'>deeper</a>"),
            "/deeper" => {
                deeper_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("<article>must not be fetched</article>")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.traversal.max_depth = 0;
    config.limits.max_pages = 5;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();

    assert_eq!(report.outcome, CrawlOutcome::Complete);
    assert_eq!(deeper_hits.load(Ordering::SeqCst), 0);
    assert_eq!(report.stats.pages_crawled, 1);
    // The link was still analyzed and counted as discovered...
    assert_eq!(report.stats.urls_discovered, 1);
    // ...but the depth gate is not a filter: nothing was rejected.
    assert_eq!(report.stats.urls_filtered, 0);
    assert_eq!(report.pages.len(), 1);
    assert_eq!(report.pages[0].links.len(), 1);
    assert!(!report.pages[0].links_truncated);
}

#[tokio::test]
async fn pages_collapsing_to_one_resource_are_crawled_once() {
    let c_hits = Arc::new(AtomicUsize::new(0));
    let (seed, server) = serve({
        let c_hits = Arc::clone(&c_hits);
        Arc::new(move |path| match path {
            "/" => Reply::ok("<a href='/a'>a</a><a href='/b'>b</a>"),
            "/a" | "/b" => Reply {
                status: "302 Found",
                headers: vec![("Location".into(), "/c".into())],
                body: String::new(),
                delay: Duration::ZERO,
                close_without_response: false,
            },
            "/c" => {
                c_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("<article>shared target</article>")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    // Sequential scheduling keeps the BFS order deterministic: /a before /b.
    config.traversal.concurrency = 1;
    config.limits.max_pages = 5;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();

    assert_eq!(report.outcome, CrawlOutcome::Complete);
    // Both redirects were fetched: dedupe happens on the final resource, not
    // on the request line, so the target server sees two hits.
    assert_eq!(c_hits.load(Ordering::SeqCst), 2);
    assert_eq!(report.stats.pages_crawled, 2);
    assert_eq!(report.stats.urls_filtered, 1);
    assert_eq!(report.pages.len(), 2);
    assert!(report.failures.is_empty());
    let collapsed = &report.pages[1];
    assert_eq!(collapsed.requested_url.path(), "/a");
    assert_eq!(collapsed.final_url.path(), "/c");
    assert_eq!(collapsed.redirect_chain.len(), 1);
    assert_eq!(collapsed.redirect_chain[0].path(), "/c");
}

#[tokio::test]
async fn query_values_are_redacted_in_reports_but_fetched_verbatim() {
    let seen_targets = Arc::new(Mutex::new(Vec::<String>::new()));
    let (base, server) = serve({
        let seen_targets = Arc::clone(&seen_targets);
        Arc::new(move |raw| {
            seen_targets.lock().unwrap().push(raw.to_string());
            match raw.split('?').next().unwrap_or_default() {
                "/" => Reply::ok("<a href='/leak?session=42&ok=1'>leak</a>"),
                "/leak" => Reply::ok("<article>linked page</article>"),
                _ => Reply::status("404 Not Found"),
            }
        })
    })
    .await;
    let seed = base.join("?token=abc").unwrap();
    let mut config = local_config();
    config.robots.respect = false;
    config.output.redact_query_values = true;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();

    assert_eq!(report.outcome, CrawlOutcome::Complete);
    assert_eq!(report.pages.len(), 2);
    // Keys survive, values are redacted, in the requested URL, the reported
    // link, and the crawled link page.
    assert_eq!(
        report.pages[0].requested_url.query(),
        Some("token=REDACTED")
    );
    assert_eq!(
        report.pages[0].links[0].url.query(),
        Some("session=REDACTED&ok=REDACTED")
    );
    assert_eq!(
        report.pages[1].requested_url.query(),
        Some("session=REDACTED&ok=REDACTED")
    );
    // Redaction is display-only: the wire saw the exact query strings.
    let seen = seen_targets.lock().unwrap();
    assert!(seen.contains(&"/?token=abc".to_string()), "{seen:?}");
    assert!(
        seen.contains(&"/leak?session=42&ok=1".to_string()),
        "{seen:?}"
    );
}

#[tokio::test]
async fn domain_scope_admits_the_same_host_on_other_ports_only() {
    let b_hits = Arc::new(AtomicUsize::new(0));
    let (other, other_server) = serve({
        let b_hits = Arc::clone(&b_hits);
        Arc::new(move |path| match path {
            "/b-ok" => {
                b_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("<article>same host, other port</article>")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let other_port = other.port().unwrap();
    let (seed, seed_server) = serve(Arc::new({
        let other = other.clone();
        move |_| {
            Reply::ok(format!(
                "<a href='{other}b-ok'>same host</a>\
                 <a href='http://localhost:{other_port}/nope'>other host</a>"
            ))
        }
    }))
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.scope.boundary = ScopeBoundary::Domain;
    config.limits.max_pages = 5;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    seed_server.abort();
    other_server.abort();

    assert_eq!(report.outcome, CrawlOutcome::Complete);
    // Same host on another port is inside the domain boundary...
    assert_eq!(b_hits.load(Ordering::SeqCst), 1);
    assert_eq!(report.stats.pages_crawled, 2);
    assert_eq!(report.stats.urls_discovered, 2);
    // ...while a different host name on the same machine is filtered before
    // any request is made.
    assert_eq!(report.stats.urls_filtered, 1);
    assert_eq!(report.stats.unique_origins, 2);
}

#[tokio::test]
async fn the_page_budget_truncates_scheduling_without_failing() {
    let two_hits = Arc::new(AtomicUsize::new(0));
    let (seed, server) = serve({
        let two_hits = Arc::clone(&two_hits);
        Arc::new(move |path| match path {
            "/" => Reply::ok("<a href='/one'>one</a>"),
            "/one" => Reply::ok("<a href='/two'>two</a>"),
            "/two" => {
                two_hits.fetch_add(1, Ordering::SeqCst);
                Reply::ok("<article>must not be fetched</article>")
            }
            _ => Reply::status("404 Not Found"),
        })
    })
    .await;
    let mut config = local_config();
    config.robots.respect = false;
    config.traversal.concurrency = 1;
    config.traversal.max_depth = 5;
    config.limits.max_pages = 2;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();

    // Hitting the page cap is a clean truncation, not a crawl failure.
    assert_eq!(report.outcome, CrawlOutcome::Complete);
    assert_eq!(report.stats.pages_crawled, 2);
    assert_eq!(report.pages.len(), 2);
    assert_eq!(two_hits.load(Ordering::SeqCst), 0);
    // Both crawled pages still had their links analyzed and discovered.
    assert_eq!(report.stats.urls_discovered, 2);
    assert_eq!(report.stats.urls_filtered, 0);
}

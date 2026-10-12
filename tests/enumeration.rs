// Enumeration-seed integration: sitemaps (urlset, nested index, gzip files),
// RSS/Atom feeds, robots enforcement, and conditional refetch of a recovered
// enumeration claim through the durable frontier.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;
use xcrawl::{
    CrawlConfig, CrawlOutcome, Crawler, DurableFrontier, EntryKind, Frontier, PortPolicy,
};

#[derive(Clone)]
struct Reply {
    status: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
        }
    }
    fn xml(body: &str) -> Self {
        Self::ok("application/xml; charset=utf-8", body.as_bytes().to_vec())
    }
    fn not_found() -> Self {
        Self {
            status: "404 Not Found",
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
    fn robots(rules: &str) -> Self {
        Self::ok("text/plain", rules.as_bytes().to_vec())
    }
}

/// Serve HTTP while capturing full request head text for conditional-header
/// assertions.
type Handler = Arc<dyn Fn(&str, &str) -> Reply + Send + Sync>;

async fn serve(handler: Handler) -> (Url, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let mut request = vec![0_u8; 16 * 1024];
                let size = stream.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..size]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                captured.lock().unwrap().push(request.clone());
                let reply = handler(&path, &request);
                let mut headers = reply.headers.clone();
                headers.push(("Content-Length".into(), reply.body.len().to_string()));
                headers.push(("Connection".into(), "close".into()));
                let mut head = format!("HTTP/1.1 {}\r\n", reply.status);
                for (name, value) in headers {
                    use std::fmt::Write as _;
                    let _ = write!(head, "{name}: {value}\r\n");
                }
                let mut response = head.into_bytes();
                response.extend_from_slice(b"\r\n");
                response.extend_from_slice(&reply.body);
                let _ = stream.write_all(&response).await;
            });
        }
    });
    (
        Url::parse(&format!("http://{address}/")).unwrap(),
        requests,
        handle,
    )
}

fn local_config() -> CrawlConfig {
    let mut config = CrawlConfig::default();
    config.network.deny_non_global = false;
    config.network.allowed_ports = PortPolicy::Any;
    config.traversal.default_delay = Duration::ZERO;
    config.traversal.max_origin_in_flight = 4;
    config.retry.base_delay = Duration::from_millis(1);
    config.retry.max_delay = Duration::from_millis(10);
    config.retry.max_attempts = 1;
    config.limits.max_crawl_duration = Duration::from_secs(10);
    config.limits.max_attempt_duration = Duration::from_secs(2);
    config.network.dns_timeout = Duration::from_secs(2);
    config.output.redact_query_values = false;
    config
}

fn hits(requests: &Arc<Mutex<Vec<String>>>, marker: &str) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.contains(marker))
        .count()
}

fn gzip_urlset(locations: &[&str]) -> Vec<u8> {
    use std::io::Write;
    let mut xml = String::from("<?xml version='1.0'?><urlset>");
    for location in locations {
        use std::fmt::Write as _;
        let _ = write!(xml, "<url><loc>{location}</loc></url>");
    }
    xml.push_str("</urlset>");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(xml.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test]
async fn sitemap_urlset_expands_into_scoped_pages() {
    let (seed, requests, server) = serve(Arc::new(|path, _| match path {
        "/robots.txt" => Reply::robots("User-agent: *\nAllow: /\n"),
        "/sitemap.xml" => Reply::xml(
            "<urlset><url><loc>/docs/one</loc></url><url><loc>/docs/two</loc></url><url><loc>/outside</loc></url></urlset>",
        ),
        "/docs/one" | "/docs/two" => Reply::ok("text/html", "<article>doc</article>"),
        "/" => Reply::ok("text/html", "<article>seed page</article>"),
        _ => Reply::not_found(),
    }))
    .await;
    let mut config = local_config();
    config.scope.exclude_path_prefixes = vec!["/outside".to_string()];
    config.enumeration.sitemaps = vec![seed.join("sitemap.xml").unwrap()];
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.outcome, CrawlOutcome::Complete);
    assert_eq!(report.stats.pages_enumerated, 1);
    let sitemap_page = report
        .pages
        .iter()
        .find(|page| page.final_url.path() == "/sitemap.xml")
        .unwrap();
    assert_eq!(sitemap_page.analysis_status, "enumerated_sitemap");
    assert_eq!(
        hits(&requests, "GET /docs/one "),
        1,
        "in-scope sitemap children must be crawled"
    );
    assert_eq!(hits(&requests, "GET /outside "), 0, "scope still filters");
}

#[tokio::test]
async fn nested_sitemap_index_expands_recursively_with_depth_bounds() {
    let (seed, requests, server) = serve(Arc::new(|path, _| match path {
        "/robots.txt" => Reply::robots("User-agent: *\nAllow: /\n"),
        "/sitemap-index.xml" => {
            Reply::xml("<sitemapindex><sitemap><loc>/sitemap-a.xml</loc></sitemap></sitemapindex>")
        }
        "/sitemap-a.xml" => Reply::xml("<urlset><url><loc>/page</loc></url></urlset>"),
        "/page" => Reply::ok("text/html", "<article>page</article>"),
        "/" => Reply::ok("text/html", "<article>seed page</article>"),
        _ => Reply::not_found(),
    }))
    .await;
    let mut config = local_config();
    config.enumeration.sitemaps = vec![seed.join("sitemap-index.xml").unwrap()];
    // Depth 1 stops after the child sitemap itself: the index (depth 0)
    // enqueues the child sitemap at depth 1, whose pages would sit at depth 2.
    config.traversal.max_depth = 1;
    let first = Crawler::new(config.clone())
        .unwrap()
        .crawl(&seed)
        .await
        .unwrap();
    assert_eq!(hits(&requests, "GET /sitemap-a.xml "), 1);
    assert_eq!(hits(&requests, "GET /page "), 0, "depth bound holds");
    // Both the index and the nested sitemap are enumerated at depth <= 1.
    assert_eq!(first.stats.pages_enumerated, 2);

    config.traversal.max_depth = 2;
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(report.stats.pages_enumerated, 2);
    let page = report
        .pages
        .iter()
        .find(|page| page.final_url.path() == "/page")
        .expect("page reachable at depth 2");
    assert_eq!(page.depth, 2);
    assert_eq!(page.analysis_status, "analyzed");
}

#[tokio::test]
async fn gzip_sitemap_files_are_inflated_regardless_of_content_type() {
    let compressed = gzip_urlset(&["/gz-page"]);
    let (seed, requests, server) = serve(Arc::new(move |path, _| match path {
        "/robots.txt" => Reply::robots("User-agent: *\nAllow: /\n"),
        "/sitemap.xml.gz" => Reply {
            status: "200 OK",
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body: compressed.clone(),
        },
        "/gz-page" => Reply::ok("text/html", "<article>gz</article>"),
        "/" => Reply::ok("text/html", "<article>seed page</article>"),
        _ => Reply::not_found(),
    }))
    .await;
    let mut config = local_config();
    config.enumeration.sitemaps = vec![seed.join("sitemap.xml.gz").unwrap()];
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(hits(&requests, "GET /gz-page "), 1);
    assert_eq!(report.stats.pages_enumerated, 1);
}

#[tokio::test]
async fn rss_and_atom_feeds_expand_into_pages() {
    let (seed, requests, server) = serve(Arc::new(|path, _| match path {
        "/robots.txt" => Reply::robots("User-agent: *\nAllow: /\n"),
        "/rss.xml" => Reply::xml(
            "<rss version='2.0'><channel><item><title>A</title><link>/from-rss</link></item></channel></rss>",
        ),
        "/atom.xml" => Reply::xml(
            "<feed xmlns='http://www.w3.org/2005/Atom'><entry><title>B</title><link rel='alternate' href='/from-atom'/></entry></feed>",
        ),
        "/from-rss" | "/from-atom" => Reply::ok("text/html", "<article>feed item</article>"),
        "/" => Reply::ok("text/html", "<article>seed page</article>"),
        _ => Reply::not_found(),
    }))
    .await;
    let mut config = local_config();
    config.enumeration.feeds = vec![
        seed.join("rss.xml").unwrap(),
        seed.join("atom.xml").unwrap(),
    ];
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(hits(&requests, "GET /from-rss "), 1);
    assert_eq!(hits(&requests, "GET /from-atom "), 1);
    assert_eq!(report.stats.pages_enumerated, 2);
    let atom_page = report
        .pages
        .iter()
        .find(|page| page.final_url.path() == "/atom.xml")
        .unwrap();
    assert_eq!(atom_page.analysis_status, "enumerated_feed");
    assert!(
        atom_page
            .links
            .iter()
            .any(|link| link.url.path() == "/from-atom" && link.text == "B")
    );
}

#[tokio::test]
async fn robots_denial_still_blocks_a_designated_sitemap() {
    let (seed, requests, server) = serve(Arc::new(|path, _| match path {
        "/robots.txt" => Reply::robots("User-agent: *\nDisallow: /sitemap.xml\n"),
        "/sitemap.xml" => Reply::xml("<urlset><url><loc>/x</loc></url></urlset>"),
        "/" => Reply::ok("text/html", "<article>seed page</article>"),
        _ => Reply::not_found(),
    }))
    .await;
    let mut config = local_config();
    config.enumeration.sitemaps = vec![seed.join("sitemap.xml").unwrap()];
    let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
    server.abort();
    assert_eq!(hits(&requests, "GET /sitemap.xml "), 0);
    assert_eq!(hits(&requests, "GET /robots.txt "), 1);
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.error.message.contains("/sitemap.xml"))
    );
}

fn private_temp_dir(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "xcrawl-enumeration-{label}-{}-{}",
        label,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

/// A recovered enumeration claim re-fetches conditionally and re-expands from
/// the retained snapshot on 304; a completed claim is never refetched.
#[tokio::test]
async fn recovered_enumeration_claim_uses_conditional_fetch_and_snapshot() {
    let sitemap_hits = Arc::new(AtomicUsize::new(0));
    let (seed, _requests, server) = serve({
        let sitemap_hits = Arc::clone(&sitemap_hits);
        Arc::new(move |path, request| {
            if path == "/sitemap.xml" {
                sitemap_hits.fetch_add(1, Ordering::SeqCst);
                if request
                    .to_ascii_lowercase()
                    .contains("if-none-match: \"v1\"")
                {
                    return Reply {
                        status: "304 Not Modified",
                        headers: vec![("ETag".into(), "\"v1\"".into())],
                        body: Vec::new(),
                    };
                }
                return Reply {
                    status: "200 OK",
                    headers: vec![
                        ("Content-Type".into(), "application/xml".into()),
                        ("ETag".into(), "\"v1\"".into()),
                    ],
                    body: b"<urlset><url><loc>/item</loc></url></urlset>".to_vec(),
                };
            }
            match path {
                "/robots.txt" => Reply::robots("User-agent: *\nAllow: /\n"),
                "/item" | "/" => Reply::ok("text/html", "<article>item</article>"),
                _ => Reply::not_found(),
            }
        })
    })
    .await;
    let sitemap_url = seed.join("sitemap.xml").unwrap();
    let mut config = local_config();
    config.enumeration.sitemaps = vec![sitemap_url.clone()];
    let output = private_temp_dir("output");
    let frontier_directory = private_temp_dir("frontier");
    config.output.private_directory = Some(output.clone());

    // Crawl 1 fetches the sitemap once, stores its validators and snapshot,
    // and marks it done.
    let crawler = Crawler::new(config.clone()).unwrap();
    let frontier: Arc<dyn Frontier> = Arc::new(
        DurableFrontier::open(&frontier_directory, config.limits.max_frontier_entries).unwrap(),
    );
    let first = crawler
        .crawl_collect_with_frontier(&seed, Arc::clone(&frontier))
        .await
        .unwrap();
    assert_eq!(first.stats.pages_enumerated, 1);
    assert_eq!(sitemap_hits.load(Ordering::SeqCst), 1);
    drop(frontier);

    // Simulate a crash after the fetch but before completion: the item is not
    // done, so reopening the frontier requeues it while validators persist.
    let checkpoint = frontier_directory.join("frontier.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    let items = state["items"].as_object().unwrap().clone();
    let key = items
        .keys()
        .find(|key| key.ends_with("/sitemap.xml"))
        .expect("sitemap item in checkpoint")
        .clone();
    state["items"][&key]["done"] = serde_json::Value::Bool(false);
    // A crashed claim still holds its lease; recovery releases and requeues it.
    state["items"][&key]["lease_until_ms"] = serde_json::json!(1);
    std::fs::write(&checkpoint, serde_json::to_vec(&state).unwrap()).unwrap();

    // Crawl 2 sends the stored validators; the 304 answer re-expands from the
    // retained snapshot instead of downloading the body again.
    let frontier: Arc<dyn Frontier> = Arc::new(
        DurableFrontier::open(&frontier_directory, config.limits.max_frontier_entries).unwrap(),
    );
    let second = crawler
        .crawl_collect_with_frontier(&seed, Arc::clone(&frontier))
        .await
        .unwrap();
    assert_eq!(second.stats.pages_enumerated, 1);
    assert_eq!(sitemap_hits.load(Ordering::SeqCst), 2);
    let recovered = second
        .pages
        .iter()
        .find(|page| page.final_url.path() == "/sitemap.xml")
        .unwrap();
    assert_eq!(recovered.status, 304);
    assert_eq!(recovered.analysis_status, "enumerated_sitemap");
    // The already-completed child is a duplicate and is deliberately not
    // refetched; the conditional document itself was not downloaded again.
    drop(frontier);

    // A completed claim is a valid resume: no third fetch of the document.
    let frontier: Arc<dyn Frontier> = Arc::new(
        DurableFrontier::open(&frontier_directory, config.limits.max_frontier_entries).unwrap(),
    );
    let third = crawler
        .crawl_collect_with_frontier(&seed, frontier)
        .await
        .unwrap();
    assert_eq!(sitemap_hits.load(Ordering::SeqCst), 2);
    assert_eq!(third.stats.pages_enumerated, 0);
    server.abort();
    let _ = std::fs::remove_dir_all(&output);
    let _ = std::fs::remove_dir_all(&frontier_directory);
}

#[tokio::test]
async fn out_of_scope_enumeration_seed_is_rejected_up_front() {
    let (seed, _requests, server) = serve(Arc::new(|path, _| match path {
        "/robots.txt" => Reply::robots("User-agent: *\nAllow: /\n"),
        _ => Reply::ok("text/html", "<article>seed</article>"),
    }))
    .await;
    let mut config = local_config();
    config.enumeration.sitemaps = vec![Url::parse("https://other.example/sitemap.xml").unwrap()];
    let error = Crawler::new(config)
        .unwrap()
        .crawl(&seed)
        .await
        .unwrap_err();
    server.abort();
    assert!(error.to_string().contains("scope"));
}

// Entry kinds are part of the durable frontier record: old checkpoints without
// the field keep working and new ones round-trip.
#[test]
fn frontier_entry_kind_round_trips_and_defaults_to_page() {
    let json = serde_json::to_value(xcrawl::FrontierEntry {
        url: Url::parse("https://example.test/s.xml").unwrap(),
        depth: 0,
        kind: EntryKind::Sitemap,
    })
    .unwrap();
    assert_eq!(json["kind"], "sitemap");
    let legacy = serde_json::json!({"url": "https://example.test/p", "depth": 1});
    let entry: xcrawl::FrontierEntry = serde_json::from_value(legacy).unwrap();
    assert_eq!(entry.kind, EntryKind::Page);
}

// Keep flate2 and serde_json linked for this test crate.
#[allow(dead_code)]
fn linked_dependencies() {
    let _ = flate2::Compression::default();
    let _ = serde_json::json!({});
}

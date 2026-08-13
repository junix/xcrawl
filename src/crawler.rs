use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use futures_util::stream::{FuturesUnordered, StreamExt};
use readabilities_rs::{PageSnapshot, Reader};
use url::Url;

use crate::fetch::{FetchedPage, Fetcher};
use crate::throttle::PerDomainThrottle;
use crate::{
    CrawlConfig, CrawlError, CrawlEvent, CrawlFailure, CrawlPage, CrawlReport, Frontier,
    FrontierEntry, InMemoryFrontier, Result, RobotsRules,
};

#[derive(Debug, Clone)]
pub struct Crawler {
    config: Arc<CrawlConfig>,
    reader: Arc<Reader>,
    fetcher: Fetcher,
    throttle: Arc<PerDomainThrottle>,
}

impl Crawler {
    pub fn new(config: CrawlConfig) -> Result<Self> {
        Self::with_reader(config, Reader::new())
    }

    /// Build a crawler around a caller-configured page analysis engine.
    ///
    /// This keeps crawl policy in `CrawlConfig` while allowing site profiles
    /// and extraction options to remain owned by `readabilities-rs`.
    pub fn with_reader(config: CrawlConfig, reader: Reader) -> Result<Self> {
        config.validate()?;
        let config = Arc::new(config);
        Ok(Self {
            reader: Arc::new(reader),
            fetcher: Fetcher::new(Arc::clone(&config)),
            throttle: Arc::new(PerDomainThrottle::new(config.default_delay)),
            config,
        })
    }

    pub async fn crawl(&self, seed: &Url) -> Result<CrawlReport> {
        self.config.validate()?;
        Self::validate_seed(seed)?;
        let started = Instant::now();
        let seed_host = seed
            .host_str()
            .ok_or_else(|| CrawlError::InvalidUrl("seed URL is missing a host".to_string()))?
            .to_ascii_lowercase();
        let frontier = InMemoryFrontier::new(self.config.strategy);
        let seed_key = normalize_url(seed);
        frontier.mark_seen(&seed_key).await?;
        frontier
            .push(FrontierEntry {
                url: seed.clone(),
                depth: 0,
            })
            .await?;

        let mut report = CrawlReport::default();
        let mut robots_cache = HashMap::<String, RobotsRules>::new();
        let mut processed = 0_usize;

        while processed < self.config.max_pages && !frontier.is_empty().await? {
            let remaining = self.config.max_pages - processed;
            let candidates = frontier
                .pop_batch(self.config.concurrency.min(remaining))
                .await?;
            let mut eligible = Vec::new();
            for entry in candidates {
                if self.config.respect_robots {
                    let rules = self.robots_for(&entry.url, &mut robots_cache).await;
                    let path = path_and_query(&entry.url);
                    if !rules.allowed(&self.config.user_agent, &path) {
                        report.stats.urls_filtered += 1;
                        continue;
                    }
                }
                eligible.push(entry);
            }
            if eligible.is_empty() {
                continue;
            }
            processed += eligible.len();

            let mut pending = FuturesUnordered::new();
            for entry in eligible {
                let fetcher = self.fetcher.clone();
                let throttle = Arc::clone(&self.throttle);
                pending.push(async move {
                    let domain = domain_key(&entry.url);
                    throttle.acquire(&domain).await;
                    let result = fetcher.fetch(&entry.url).await;
                    match &result {
                        Ok(page) => throttle.record_response(&domain, page.status),
                        Err(CrawlError::HttpStatus { status, .. }) => {
                            throttle.record_response(&domain, *status);
                        }
                        Err(_) => {}
                    }
                    (entry, result)
                });
            }

            while let Some((entry, fetched)) = pending.next().await {
                match fetched {
                    Ok(fetched) => {
                        self.process_page(entry, fetched, &seed_host, &frontier, &mut report)
                            .await?;
                    }
                    Err(error) => {
                        report.events.push(CrawlEvent::Failed {
                            url: entry.url.clone(),
                            depth: entry.depth,
                            error: error.to_string(),
                        });
                        report.failures.push(CrawlFailure {
                            url: entry.url,
                            depth: entry.depth,
                            error: error.to_string(),
                        });
                    }
                }
            }
        }

        report.stats.pages_crawled = report.pages.len();
        report.stats.pages_failed = report.failures.len();
        report.stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        report.events.push(CrawlEvent::Complete {
            stats: report.stats.clone(),
        });
        Ok(report)
    }

    /// Validate a seed without making a network request.
    pub fn validate_seed(seed: &Url) -> Result<()> {
        validate_seed(seed)
    }

    async fn process_page(
        &self,
        entry: FrontierEntry,
        fetched: FetchedPage,
        seed_host: &str,
        frontier: &InMemoryFrontier,
        report: &mut CrawlReport,
    ) -> Result<()> {
        let mut snapshot = PageSnapshot::origin(
            fetched.final_url.clone(),
            fetched.content_type.clone(),
            fetched.body.clone(),
        );
        snapshot.response_headers = fetched.response_headers;
        let analysis = self.reader.analyze_snapshot(snapshot);
        let (article, article_error) = match analysis.article {
            Ok(article) => (Some(article), None),
            Err(error) => (None, Some(error)),
        };
        let page = CrawlPage {
            requested_url: entry.url.clone(),
            final_url: fetched.final_url.clone(),
            depth: entry.depth,
            status: fetched.status,
            content_type: fetched.content_type,
            body_bytes: fetched.body.len(),
            detected_encoding: analysis.detected_encoding,
            decode_errors: analysis.decode_errors,
            canonical_url: analysis.canonical_url,
            robots: analysis.robots,
            links: analysis.links,
            article,
            article_error,
        };

        report.events.push(CrawlEvent::Page {
            url: page.final_url.clone(),
            depth: entry.depth,
            status: page.status,
        });

        if entry.depth < self.config.max_depth && !page.robots.nofollow {
            for link in page.links.iter().take(self.config.max_links_per_page) {
                report.stats.urls_discovered += 1;
                if link.nofollow && !self.config.follow_nofollow {
                    report.stats.urls_filtered += 1;
                    continue;
                }
                if !self.in_scope(&link.url, seed_host, entry.depth + 1) {
                    report.stats.urls_filtered += 1;
                    continue;
                }
                let key = normalize_url(&link.url);
                if !frontier.mark_seen(&key).await? {
                    report.stats.urls_filtered += 1;
                    continue;
                }
                let url =
                    Url::parse(&key).map_err(|error| CrawlError::InvalidUrl(error.to_string()))?;
                frontier
                    .push(FrontierEntry {
                        url: url.clone(),
                        depth: entry.depth + 1,
                    })
                    .await?;
                report.events.push(CrawlEvent::Discovered {
                    url,
                    depth: entry.depth + 1,
                });
            }
        }
        report.pages.push(page);
        Ok(())
    }

    async fn robots_for(&self, url: &Url, cache: &mut HashMap<String, RobotsRules>) -> RobotsRules {
        let key = origin_key(url);
        if let Some(rules) = cache.get(&key) {
            return rules.clone();
        }
        let rules = match url.join("/robots.txt") {
            Ok(robots_url) => {
                let domain = domain_key(&robots_url);
                self.throttle.acquire(&domain).await;
                match self.fetcher.fetch(&robots_url).await {
                    Ok(page) => RobotsRules::parse(&String::from_utf8_lossy(&page.body)),
                    Err(_) => RobotsRules::default(),
                }
            }
            Err(_) => RobotsRules::default(),
        };
        if let Some(delay) = rules.crawl_delay(&self.config.user_agent) {
            self.throttle.set_robots_delay(&domain_key(url), delay);
        }
        cache.insert(key, rules.clone());
        rules
    }

    fn in_scope(&self, url: &Url, seed_host: &str, depth: usize) -> bool {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return false;
        }
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        if self.config.stay_on_domain
            && host != seed_host
            && !(self.config.allow_subdomains && host.ends_with(&format!(".{seed_host}")))
        {
            return false;
        }
        let path = url.path();
        if self
            .config
            .exclude_path_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            return false;
        }
        depth == 0
            || self.config.include_path_prefixes.is_empty()
            || self
                .config
                .include_path_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix))
    }
}

fn validate_seed(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(CrawlError::InvalidUrl(
            "seed must be an absolute HTTP(S) URL".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CrawlError::InvalidUrl(
            "embedded URL credentials are not allowed".to_string(),
        ));
    }
    Ok(())
}

fn normalize_url(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    url.to_string()
}

fn path_and_query(url: &Url) -> String {
    url.query().map_or_else(
        || url.path().to_string(),
        |query| format!("{}?{query}", url.path()),
    )
}

fn origin_key(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    )
}

fn domain_key(url: &Url) -> String {
    format!(
        "{}:{}",
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn serve_site() -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut request = vec![0_u8; 4096];
                    let size = stream.read(&mut request).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..size]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_ascii_whitespace().nth(1))
                        .unwrap_or("/");
                    let body = match path {
                        "/robots.txt" => "User-agent: *\nAllow: /\nCrawl-delay: 0\n".to_string(),
                        "/" => "<html><body><article><h1>Root</h1><p>Root article content.</p></article><nav><a href='/article'>article</a><a href='https://outside.test/x'>outside</a></nav></body></html>".to_string(),
                        "/article" => "<html><body><article><h1>Article</h1><p>REQUIRED-XCRAWL article body.</p></article><a href='/empty'>empty</a></body></html>".to_string(),
                        "/empty" => "<html><body><nav><a href='/third'>third</a></nav></body></html>".to_string(),
                        "/third" => "<html><body><article><h1>Third</h1><p>REQUIRED-THIRD reached through a page with no readable article.</p></article></body></html>".to_string(),
                        _ => "not found".to_string(),
                    };
                    let status = if path == "/missing" {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), handle)
    }

    #[tokio::test]
    async fn one_snapshot_drives_article_and_frontier_progress() {
        let (seed, server) = serve_site().await;
        let config = CrawlConfig {
            max_depth: 3,
            max_pages: 10,
            concurrency: 2,
            default_delay: std::time::Duration::ZERO,
            allow_private_networks: true,
            ..CrawlConfig::default()
        };
        let report = Crawler::new(config).unwrap().crawl(&seed).await.unwrap();
        server.abort();

        assert_eq!(report.failures.len(), 0, "{:?}", report.failures);
        assert_eq!(report.pages.len(), 4);
        let article = report
            .pages
            .iter()
            .find(|page| page.final_url.path() == "/article")
            .and_then(|page| page.article.as_ref())
            .unwrap();
        assert!(article.content.contains("REQUIRED-XCRAWL"));
        let empty = report
            .pages
            .iter()
            .find(|page| page.final_url.path() == "/empty")
            .unwrap();
        assert!(empty.article.is_none());
        assert!(empty.article_error.is_some());
        assert!(
            report
                .pages
                .iter()
                .any(|page| page.final_url.path() == "/third")
        );
        assert!(report.stats.urls_filtered >= 1);
    }

    #[test]
    fn subdomain_matching_requires_a_label_boundary() {
        let config = CrawlConfig {
            allow_subdomains: true,
            ..CrawlConfig::default()
        };
        let crawler = Crawler::new(config).unwrap();
        assert!(crawler.in_scope(
            &Url::parse("https://docs.example.test/a").unwrap(),
            "example.test",
            1
        ));
        assert!(!crawler.in_scope(
            &Url::parse("https://notexample.test/a").unwrap(),
            "example.test",
            1
        ));
    }

    #[allow(dead_code)]
    fn _assert_socket_addr_is_send(_: SocketAddr) {}
}

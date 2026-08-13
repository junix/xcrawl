use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use tokio::time::Instant;
use url::Url;

use crate::analyzer::{PageAnalyzer, PageInput, ReadabilitiesAnalyzer};
use crate::budget::CrawlBudget;
use crate::fetch::{HopOutcome, HopResponse, OneHopTransport, origin_key, safe_url, validate_url};
use crate::frontier::normalize_url;
use crate::robots::{RobotsRules, RobotsState};
use crate::throttle::OriginScheduler;
use crate::{
    CrawlConfig, CrawlError, CrawlEvent, CrawlFailure, CrawlFailureDetail, CrawlOutcome, CrawlPage,
    CrawlRecord, CrawlReport, CrawlSink, CrawlSinkError, CrawlStats, CrawlSummary, FailureKind,
    Frontier, FrontierEntry, InMemoryFrontier, NullCrawlSink, PageAnalysis, PathMatchMode,
    RedirectPolicy, RequestKind, Result, ScopeBoundary,
};

#[derive(Clone)]
pub struct Crawler {
    config: Arc<CrawlConfig>,
    analyzer: Arc<dyn PageAnalyzer>,
    transport: OneHopTransport,
}

impl fmt::Debug for Crawler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Crawler")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Crawler {
    pub fn new(config: CrawlConfig) -> Result<Self> {
        Self::with_analyzer(config, Arc::new(ReadabilitiesAnalyzer::default()))
    }

    pub fn with_analyzer(config: CrawlConfig, analyzer: Arc<dyn PageAnalyzer>) -> Result<Self> {
        config.validate()?;
        let config = Arc::new(config);
        let transport = OneHopTransport::new(&config)?;
        Ok(Self {
            config,
            analyzer,
            transport,
        })
    }

    /// Crawl into a bounded in-memory report.
    pub async fn crawl(&self, seed: &Url) -> Result<CrawlReport> {
        let frontier: Arc<dyn Frontier> = Arc::new(InMemoryFrontier::new(
            self.config.traversal.strategy,
            self.config.limits.max_frontier_entries,
        ));
        self.crawl_collect_with_frontier(seed, frontier).await
    }

    /// Crawl into an injected frontier while collecting a bounded report.
    pub async fn crawl_collect_with_frontier(
        &self,
        seed: &Url,
        frontier: Arc<dyn Frontier>,
    ) -> Result<CrawlReport> {
        let output = self
            .run(seed, frontier, Arc::new(NullCrawlSink), true)
            .await?;
        Ok(output.report)
    }

    /// Stream crawl records without accumulating pages, failures, or events.
    pub async fn crawl_with_sink(
        &self,
        seed: &Url,
        sink: Arc<dyn CrawlSink>,
    ) -> Result<CrawlSummary> {
        let frontier: Arc<dyn Frontier> = Arc::new(InMemoryFrontier::new(
            self.config.traversal.strategy,
            self.config.limits.max_frontier_entries,
        ));
        let output = self.run(seed, frontier, sink, false).await?;
        Ok(output.summary)
    }

    pub fn validate_seed(&self, seed: &Url) -> Result<()> {
        validate_url(seed, &self.config.network)?;
        let scope = ScopeContext::new(seed)?;
        if !in_scope(&self.config, &scope, seed) {
            return Err(CrawlError::InvalidUrl(
                "seed URL is excluded by the configured scope policy".to_string(),
            ));
        }
        if seed.as_str().len() > self.config.limits.max_url_length {
            return Err(CrawlError::ResourceBudget {
                resource: "url_length",
                limit: self.config.limits.max_url_length,
            });
        }
        Ok(())
    }

    async fn run(
        &self,
        seed: &Url,
        frontier: Arc<dyn Frontier>,
        sink: Arc<dyn CrawlSink>,
        collect: bool,
    ) -> Result<RunOutput> {
        self.config.validate()?;
        self.validate_seed(seed)?;
        let started = Instant::now();
        let scope = Arc::new(ScopeContext::new(seed)?);
        let budget = Arc::new(CrawlBudget::new(Arc::clone(&self.config))?);
        if collect {
            budget.reserve_report_bytes(1_024)?;
        }
        let runtime = Arc::new(CrawlRuntime {
            config: Arc::clone(&self.config),
            analyzer: Arc::clone(&self.analyzer),
            transport: self.transport.clone(),
            throttle: Arc::new(OriginScheduler::new(
                self.config.traversal.default_delay,
                self.config.traversal.max_origin_in_flight,
            )),
            budget: Arc::clone(&budget),
            robots: Arc::new(AsyncMutex::new(HashMap::new())),
            resources_seen: Arc::new(Mutex::new(HashSet::new())),
            scope,
        });

        let seed_result = frontier
            .enqueue_if_new(vec![FrontierEntry {
                url: seed.clone(),
                depth: 0,
            }])
            .await?;
        if seed_result.enqueued.is_empty() {
            return Err(CrawlError::ResourceBudget {
                resource: "frontier_entries",
                limit: self.config.limits.max_frontier_entries,
            });
        }

        let mut report = CrawlReport::default();
        let mut pending = FuturesUnordered::new();
        let mut scheduled = 0_usize;
        let mut terminal_outcome = None;
        let mut termination_reason = None;
        let mut sink_closed = false;

        loop {
            while !sink_closed
                && terminal_outcome.is_none()
                && pending.len() < self.config.traversal.concurrency
                && scheduled < self.config.limits.max_pages
            {
                if let Err(error) = budget.check_deadline() {
                    terminal_outcome = Some(CrawlOutcome::DeadlineExceeded);
                    termination_reason = Some(error.to_string());
                    break;
                }
                let Some(entry) = frontier.pop().await? else {
                    break;
                };
                scheduled += 1;
                let runtime = Arc::clone(&runtime);
                pending.push(async move { runtime.fetch_and_analyze(entry).await });
            }

            if pending.is_empty() {
                if terminal_outcome.is_some()
                    || sink_closed
                    || scheduled >= self.config.limits.max_pages
                    || frontier.is_empty().await?
                {
                    break;
                }
                continue;
            }

            let remaining = match budget.remaining_duration() {
                Ok(remaining) => remaining,
                Err(error) => {
                    terminal_outcome = Some(CrawlOutcome::DeadlineExceeded);
                    termination_reason = Some(error.to_string());
                    break;
                }
            };
            let completed = tokio::time::timeout(remaining, pending.next()).await;
            let Some(task) = (if let Ok(task) = completed {
                task
            } else {
                terminal_outcome = Some(CrawlOutcome::DeadlineExceeded);
                termination_reason = Some(CrawlError::DeadlineExceeded.to_string());
                break;
            }) else {
                continue;
            };

            for event in task.events() {
                match emit_record(
                    &sink,
                    &budget,
                    &mut report,
                    CrawlRecord::Event {
                        value: event.clone(),
                    },
                    collect,
                    self.config.output.collect_events,
                )? {
                    EmitState::Continue => {}
                    EmitState::BrokenPipe => {
                        sink_closed = true;
                        terminal_outcome = Some(CrawlOutcome::Cancelled);
                        termination_reason = Some("output consumer closed the stream".to_string());
                        break;
                    }
                }
            }
            if sink_closed {
                break;
            }

            match task {
                PageTask::Success(success) => {
                    let processed = self
                        .process_analysis(*success, &frontier, &mut report.stats)
                        .await?;
                    for discovered in processed.discovered {
                        let event = CrawlEvent::Discovered {
                            url: report_url(
                                &discovered.url,
                                self.config.output.redact_query_values,
                            ),
                            depth: discovered.depth,
                        };
                        if emit_record(
                            &sink,
                            &budget,
                            &mut report,
                            CrawlRecord::Event { value: event },
                            collect,
                            self.config.output.collect_events,
                        )? == EmitState::BrokenPipe
                        {
                            sink_closed = true;
                            terminal_outcome = Some(CrawlOutcome::Cancelled);
                            termination_reason =
                                Some("output consumer closed the stream".to_string());
                            break;
                        }
                    }
                    let page = processed.page;
                    report.stats.pages_crawled = report.stats.pages_crawled.saturating_add(1);
                    let event = CrawlEvent::Page {
                        url: page.final_url.clone(),
                        depth: page.depth,
                        status: page.status,
                    };
                    if emit_record(
                        &sink,
                        &budget,
                        &mut report,
                        CrawlRecord::Event { value: event },
                        collect,
                        self.config.output.collect_events,
                    )? == EmitState::BrokenPipe
                        || emit_record(
                            &sink,
                            &budget,
                            &mut report,
                            CrawlRecord::Page {
                                value: Box::new(page),
                            },
                            collect,
                            self.config.output.collect_events,
                        )? == EmitState::BrokenPipe
                    {
                        sink_closed = true;
                        terminal_outcome = Some(CrawlOutcome::Cancelled);
                        termination_reason = Some("output consumer closed the stream".to_string());
                    }
                }
                PageTask::Failure(failure) => {
                    let failure = failure.into_model(self.config.output.redact_query_values);
                    report.stats.pages_failed = report.stats.pages_failed.saturating_add(1);
                    let event = CrawlEvent::Failed {
                        url: failure.url.clone(),
                        depth: failure.depth,
                        error: failure.error.clone(),
                    };
                    if failure.depth == 0 {
                        terminal_outcome = Some(CrawlOutcome::SeedFailed);
                        termination_reason = Some(failure.error.message.clone());
                    }
                    if emit_record(
                        &sink,
                        &budget,
                        &mut report,
                        CrawlRecord::Event { value: event },
                        collect,
                        self.config.output.collect_events,
                    )? == EmitState::BrokenPipe
                        || emit_record(
                            &sink,
                            &budget,
                            &mut report,
                            CrawlRecord::Failure { value: failure },
                            collect,
                            self.config.output.collect_events,
                        )? == EmitState::BrokenPipe
                    {
                        sink_closed = true;
                        terminal_outcome = Some(CrawlOutcome::Cancelled);
                        termination_reason = Some("output consumer closed the stream".to_string());
                    }
                }
                PageTask::Duplicate { .. } => {
                    report.stats.urls_filtered += 1;
                }
            }
        }

        drop(pending);
        let budget_snapshot = budget.snapshot();
        report.stats.http_requests = budget_snapshot.requests;
        report.stats.downloaded_bytes = budget_snapshot.bytes;
        report.stats.unique_origins = budget_snapshot.origins;
        report.stats.elapsed_ms = millis(started.elapsed());

        let outcome = terminal_outcome.unwrap_or({
            if report.stats.pages_failed == 0 {
                CrawlOutcome::Complete
            } else if report.stats.pages_crawled == 0 {
                CrawlOutcome::SeedFailed
            } else {
                CrawlOutcome::Partial
            }
        });
        report.outcome = outcome;
        report.termination_reason = termination_reason.clone();
        let summary = CrawlSummary {
            outcome,
            termination_reason,
            stats: report.stats.clone(),
        };
        let complete = CrawlEvent::Complete {
            outcome,
            stats: report.stats.clone(),
        };
        if !sink_closed {
            let _ = emit_record(
                &sink,
                &budget,
                &mut report,
                CrawlRecord::Event { value: complete },
                collect,
                self.config.output.collect_events,
            )?;
            let _ = emit_record(
                &sink,
                &budget,
                &mut report,
                CrawlRecord::Summary {
                    value: summary.clone(),
                },
                false,
                false,
            )?;
        }
        Ok(RunOutput { report, summary })
    }

    async fn process_analysis(
        &self,
        mut success: PageSuccess,
        frontier: &Arc<dyn Frontier>,
        stats: &mut CrawlStats,
    ) -> Result<ProcessedPage> {
        let links_discovered = success.analysis.links_discovered;
        stats.urls_discovered = stats.urls_discovered.saturating_add(links_discovered);
        success
            .analysis
            .links
            .truncate(self.config.traversal.max_links_to_analyze);

        if success.entry.depth < self.config.traversal.max_depth
            && !success.analysis.robots.nofollow
        {
            let mut candidates = Vec::new();
            for link in &success.analysis.links {
                if candidates.len() >= self.config.traversal.max_links_to_enqueue {
                    break;
                }
                if link.nofollow && !self.config.traversal.follow_nofollow {
                    stats.urls_filtered += 1;
                    continue;
                }
                if link.url.as_str().len() > self.config.limits.max_url_length
                    || !in_scope(&self.config, &success.scope, &link.url)
                {
                    stats.urls_filtered += 1;
                    continue;
                }
                candidates.push(FrontierEntry {
                    url: link.url.clone(),
                    depth: success.entry.depth + 1,
                });
            }
            let result = frontier.enqueue_if_new(candidates).await?;
            stats.urls_filtered = stats
                .urls_filtered
                .saturating_add(result.duplicates)
                .saturating_add(result.rejected_capacity);
            success.enqueued = result.enqueued;
        }

        let mut links = success.analysis.links;
        let links_truncated = links_discovered > self.config.traversal.max_links_to_report;
        links.truncate(self.config.traversal.max_links_to_report);
        if self.config.output.redact_query_values {
            for link in &mut links {
                link.url = sanitized_url(&link.url);
            }
        }
        let page = CrawlPage {
            requested_url: report_url(&success.entry.url, self.config.output.redact_query_values),
            final_url: report_url(
                &success.response.url,
                self.config.output.redact_query_values,
            ),
            redirect_chain: success
                .redirect_chain
                .iter()
                .map(|url| report_url(url, self.config.output.redact_query_values))
                .collect(),
            depth: success.entry.depth,
            status: success.response.status,
            content_type: success.response.headers.content_type,
            body_bytes: success.body_bytes,
            detected_encoding: success.analysis.detected_encoding,
            decode_errors: success.analysis.decode_errors,
            canonical_url: success
                .analysis
                .canonical_url
                .map(|url| report_url(&url, self.config.output.redact_query_values)),
            robots: success.analysis.robots,
            links_discovered,
            links_truncated,
            links,
            article: success.analysis.article,
            article_error: success.analysis.article_error,
        };
        Ok(ProcessedPage {
            page,
            discovered: success.enqueued,
        })
    }
}

#[derive(Clone)]
struct CrawlRuntime {
    config: Arc<CrawlConfig>,
    analyzer: Arc<dyn PageAnalyzer>,
    transport: OneHopTransport,
    throttle: Arc<OriginScheduler>,
    budget: Arc<CrawlBudget>,
    robots: Arc<AsyncMutex<HashMap<String, Arc<OnceCell<CachedRobots>>>>>,
    resources_seen: Arc<Mutex<HashSet<String>>>,
    scope: Arc<ScopeContext>,
}

impl CrawlRuntime {
    async fn fetch_and_analyze(&self, entry: FrontierEntry) -> PageTask {
        let request_key = normalize_url(&entry.url);
        if self
            .resources_seen
            .lock()
            .expect("resource seen lock poisoned")
            .contains(&request_key)
        {
            return PageTask::Duplicate { events: Vec::new() };
        }
        let fetched = match self.fetch_page(&entry).await {
            Ok(fetched) => fetched,
            Err(failure) => return PageTask::Failure(failure),
        };
        let resource_key = normalize_url(&fetched.response.url);
        if !self
            .resources_seen
            .lock()
            .expect("resource seen lock poisoned")
            .insert(resource_key)
        {
            return PageTask::Duplicate {
                events: fetched.events,
            };
        }

        let HopResponse {
            url,
            status,
            headers,
            body,
        } = fetched.response;
        let body_bytes = body.len();
        let input = PageInput {
            final_url: url.clone(),
            content_type: headers.content_type.clone(),
            body,
            response_headers: headers.values.clone(),
            max_links: self.config.traversal.max_links_to_analyze,
        };
        let analyzer = Arc::clone(&self.analyzer);
        let analysis = match tokio::task::spawn_blocking(move || analyzer.analyze(input)).await {
            Ok(analysis) => analysis,
            Err(error) => {
                return PageTask::Failure(TaskFailure {
                    entry,
                    error: CrawlError::Analysis(error.to_string()),
                    attempts: fetched.attempts,
                    redirect_chain: fetched.redirect_chain,
                    events: fetched.events,
                });
            }
        };
        PageTask::Success(Box::new(PageSuccess {
            entry,
            response: HopResponse {
                url,
                status,
                headers,
                body: Vec::new(),
            },
            body_bytes,
            redirect_chain: fetched.redirect_chain,
            events: fetched.events,
            analysis,
            scope: Arc::clone(&self.scope),
            enqueued: Vec::new(),
        }))
    }

    async fn fetch_page(&self, entry: &FrontierEntry) -> std::result::Result<Fetched, TaskFailure> {
        let mut current = entry.url.clone();
        let initial_origin = origin_key(&current);
        let mut redirects = Vec::new();
        let mut events = Vec::new();
        let mut attempts = 0_usize;

        loop {
            if let Err(error) = self.validate_page_target(&current, &initial_origin) {
                return Err(TaskFailure {
                    entry: entry.clone(),
                    error,
                    attempts,
                    redirect_chain: redirects,
                    events,
                });
            }
            if self.config.robots.respect {
                let (robots, mut robots_events) = self.robots_for(&current).await;
                events.append(&mut robots_events);
                if let Some(delay) = robots.crawl_delay(&self.config.network.user_agent) {
                    self.throttle.set_robots_delay(&origin_key(&current), delay);
                }
                if !robots.allowed(&self.config.network.user_agent, &path_and_query(&current)) {
                    return Err(TaskFailure {
                        entry: entry.clone(),
                        error: CrawlError::RobotsDenied(safe_url(&current)),
                        attempts,
                        redirect_chain: redirects,
                        events,
                    });
                }
            }

            let attempt = self
                .send_with_retries(
                    &current,
                    RequestKind::Page,
                    self.config.limits.max_response_bytes,
                )
                .await;
            let (outcome, used_attempts, mut attempt_events) = match attempt {
                Ok(result) => result,
                Err(failure) => {
                    attempts = attempts.saturating_add(usize::from(failure.attempts));
                    events.extend(failure.events);
                    return Err(TaskFailure {
                        entry: entry.clone(),
                        error: failure.error,
                        attempts,
                        redirect_chain: redirects,
                        events,
                    });
                }
            };
            attempts = attempts.saturating_add(usize::from(used_attempts));
            events.append(&mut attempt_events);
            match outcome {
                HopOutcome::Response(response) if (200..300).contains(&response.status) => {
                    return Ok(Fetched {
                        response,
                        attempts,
                        redirect_chain: redirects,
                        events,
                    });
                }
                HopOutcome::Response(response) => {
                    return Err(TaskFailure {
                        entry: entry.clone(),
                        error: CrawlError::HttpStatus {
                            status: response.status,
                            url: safe_url(&response.url),
                        },
                        attempts,
                        redirect_chain: redirects,
                        events,
                    });
                }
                HopOutcome::Redirect { location, .. } => {
                    if redirects.len() >= usize::from(self.config.scope.max_redirects) {
                        return Err(TaskFailure {
                            entry: entry.clone(),
                            error: CrawlError::RedirectBudget,
                            attempts,
                            redirect_chain: redirects,
                            events,
                        });
                    }
                    let next = match current.join(&location) {
                        Ok(next) => next,
                        Err(error) => {
                            return Err(TaskFailure {
                                entry: entry.clone(),
                                error: CrawlError::InvalidUrl(error.to_string()),
                                attempts,
                                redirect_chain: redirects,
                                events,
                            });
                        }
                    };
                    if let Err(error) = self.validate_redirect(&current, &next, &initial_origin) {
                        return Err(TaskFailure {
                            entry: entry.clone(),
                            error,
                            attempts,
                            redirect_chain: redirects,
                            events,
                        });
                    }
                    redirects.push(next.clone());
                    current = next;
                }
            }
        }
    }

    fn validate_page_target(&self, url: &Url, initial_origin: &str) -> Result<()> {
        validate_url(url, &self.config.network)?;
        self.budget.check_deadline()?;
        if url.as_str().len() > self.config.limits.max_url_length {
            return Err(CrawlError::ResourceBudget {
                resource: "url_length",
                limit: self.config.limits.max_url_length,
            });
        }
        match self.config.scope.redirect_policy {
            RedirectPolicy::WithinCrawlScope if !in_scope(&self.config, &self.scope, url) => {
                Err(CrawlError::RedirectDenied(format!(
                    "target {} is outside crawl scope",
                    safe_url(url)
                )))
            }
            RedirectPolicy::SameOrigin if origin_key(url) != initial_origin => Err(
                CrawlError::RedirectDenied("target changed origin".to_string()),
            ),
            _ => Ok(()),
        }
    }

    fn validate_redirect(&self, current: &Url, next: &Url, initial_origin: &str) -> Result<()> {
        validate_url(next, &self.config.network)?;
        if current.scheme() == "https"
            && next.scheme() == "http"
            && !self.config.scope.allow_https_downgrade
        {
            return Err(CrawlError::RedirectDenied(
                "HTTPS-to-HTTP downgrade is disabled".to_string(),
            ));
        }
        self.validate_page_target(next, initial_origin)
    }

    async fn robots_for(&self, url: &Url) -> (RobotsState, Vec<CrawlEvent>) {
        let key = origin_key(url);
        let cell = {
            let mut cache = self.robots.lock().await;
            Arc::clone(
                cache
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        let base = url.clone();
        let cached = cell
            .get_or_init(|| async { self.fetch_robots(&base, &key).await })
            .await;
        let events = if cached.events_claimed.swap(true, Ordering::AcqRel) {
            Vec::new()
        } else {
            cached.events.clone()
        };
        (cached.state.clone(), events)
    }

    async fn fetch_robots(&self, base: &Url, initial_origin: &str) -> CachedRobots {
        let mut current = base.clone();
        current.set_path("/robots.txt");
        current.set_query(None);
        current.set_fragment(None);
        let mut redirects = 0_u8;
        let mut events = Vec::new();
        let state = loop {
            if let Err(error) = validate_url(&current, &self.config.network) {
                break RobotsState::UnreachableDisallow {
                    status: None,
                    error: error.to_string(),
                };
            }
            if current.as_str().len() > self.config.limits.max_url_length {
                break RobotsState::UnreachableDisallow {
                    status: None,
                    error: CrawlError::ResourceBudget {
                        resource: "url_length",
                        limit: self.config.limits.max_url_length,
                    }
                    .to_string(),
                };
            }
            let attempt = self
                .send_with_retries(
                    &current,
                    RequestKind::Robots,
                    self.config.limits.max_robots_bytes,
                )
                .await;
            let (outcome, _, mut attempt_events) = match attempt {
                Ok(result) => result,
                Err(failure) => {
                    events.extend(failure.events);
                    break RobotsState::UnreachableDisallow {
                        status: failure.error.status(),
                        error: failure.error.to_string(),
                    };
                }
            };
            events.append(&mut attempt_events);
            match outcome {
                HopOutcome::Response(response) if (200..300).contains(&response.status) => {
                    break RobotsState::Rules {
                        rules: Arc::new(RobotsRules::parse_with_max_delay(
                            &String::from_utf8_lossy(&response.body),
                            self.config.robots.max_delay,
                        )),
                        status: response.status,
                    };
                }
                HopOutcome::Response(response) if (400..500).contains(&response.status) => {
                    break RobotsState::UnavailableAllow {
                        status: response.status,
                    };
                }
                HopOutcome::Response(response) => {
                    break RobotsState::UnreachableDisallow {
                        status: Some(response.status),
                        error: format!("robots.txt returned HTTP {}", response.status),
                    };
                }
                HopOutcome::Redirect { location, .. } => {
                    if redirects >= self.config.robots.max_redirects {
                        break RobotsState::UnreachableDisallow {
                            status: None,
                            error: CrawlError::RedirectBudget.to_string(),
                        };
                    }
                    let Ok(next) = current.join(&location) else {
                        break RobotsState::UnreachableDisallow {
                            status: None,
                            error: "robots redirect contained an invalid Location".to_string(),
                        };
                    };
                    if current.scheme() == "https"
                        && next.scheme() == "http"
                        && !self.config.scope.allow_https_downgrade
                    {
                        break RobotsState::UnreachableDisallow {
                            status: None,
                            error: "robots redirect attempted an HTTPS downgrade".to_string(),
                        };
                    }
                    redirects = redirects.saturating_add(1);
                    current = next;
                }
            }
        };
        if let Some(delay) = state.crawl_delay(&self.config.network.user_agent) {
            self.throttle.set_robots_delay(initial_origin, delay);
        }
        let (decision, status, error) = state.event_fields();
        tracing::info!(
            origin = %initial_origin,
            decision = ?decision,
            status,
            "robots decision cached"
        );
        events.push(CrawlEvent::Robots {
            origin: initial_origin.to_string(),
            decision,
            status,
            error,
        });
        CachedRobots {
            state,
            events,
            events_claimed: AtomicBool::new(false),
        }
    }

    async fn send_with_retries(
        &self,
        url: &Url,
        kind: RequestKind,
        max_body_bytes: usize,
    ) -> std::result::Result<(HopOutcome, u8, Vec<CrawlEvent>), AttemptFailure> {
        let origin = origin_key(url);
        let mut events = Vec::new();
        for attempt in 1..=self.config.retry.max_attempts {
            if let Err(error) = self.budget.reserve_request(url, &origin) {
                return Err(AttemptFailure {
                    error,
                    attempts: attempt.saturating_sub(1),
                    events,
                });
            }
            let permit = self.throttle.acquire(&origin).await;
            let started = Instant::now();
            let timeout = match self.budget.remaining_duration() {
                Ok(remaining) => remaining.min(self.config.limits.max_attempt_duration),
                Err(error) => {
                    return Err(AttemptFailure {
                        error,
                        attempts: attempt.saturating_sub(1),
                        events,
                    });
                }
            };
            let result = tokio::time::timeout(
                timeout,
                self.transport
                    .send_one_hop(url, max_body_bytes, &self.budget),
            )
            .await
            .map_err(|_| CrawlError::AttemptTimeout)
            .and_then(std::convert::identity);
            drop(permit);

            let (status, bytes, retry_after) = match &result {
                Ok(HopOutcome::Response(response)) => (
                    Some(response.status),
                    response.body.len(),
                    response.headers.retry_after,
                ),
                Ok(HopOutcome::Redirect {
                    status, headers, ..
                }) => (Some(*status), 0, headers.retry_after),
                Err(_) => (None, 0, None),
            };
            if let Some(status) = status {
                self.throttle.record_response(
                    &origin,
                    status,
                    self.config
                        .retry
                        .honor_retry_after
                        .then_some(retry_after)
                        .flatten(),
                );
            }
            events.push(CrawlEvent::Request {
                kind,
                url: report_url(url, self.config.output.redact_query_values),
                attempt,
                status,
                bytes,
                elapsed_ms: millis(started.elapsed()),
            });
            tracing::debug!(
                kind = ?kind,
                url = %safe_url(url),
                attempt,
                status,
                bytes,
                elapsed_ms = millis(started.elapsed()),
                "HTTP attempt completed"
            );

            let retryable = match &result {
                Ok(HopOutcome::Response(response)) => {
                    matches!(response.status, 408 | 425 | 429 | 500..=599)
                }
                Ok(HopOutcome::Redirect { .. }) => false,
                Err(error) => error.retryable(),
            };
            if !retryable || attempt >= self.config.retry.max_attempts {
                return match result {
                    Ok(outcome) => Ok((outcome, attempt, events)),
                    Err(error) => Err(AttemptFailure {
                        error,
                        attempts: attempt,
                        events,
                    }),
                };
            }

            let delay = retry_delay(
                &self.config,
                url,
                attempt,
                self.config
                    .retry
                    .honor_retry_after
                    .then_some(retry_after)
                    .flatten(),
            );
            let remaining = match self.budget.remaining_duration() {
                Ok(remaining) => remaining,
                Err(error) => {
                    return Err(AttemptFailure {
                        error,
                        attempts: attempt,
                        events,
                    });
                }
            };
            tokio::time::sleep(delay.min(remaining)).await;
        }
        unreachable!("validated retry policy always has at least one attempt")
    }
}

struct CachedRobots {
    state: RobotsState,
    events: Vec<CrawlEvent>,
    events_claimed: AtomicBool,
}

struct Fetched {
    response: HopResponse,
    attempts: usize,
    redirect_chain: Vec<Url>,
    events: Vec<CrawlEvent>,
}

struct PageSuccess {
    entry: FrontierEntry,
    response: HopResponse,
    body_bytes: usize,
    redirect_chain: Vec<Url>,
    events: Vec<CrawlEvent>,
    analysis: PageAnalysis,
    scope: Arc<ScopeContext>,
    enqueued: Vec<FrontierEntry>,
}

struct ProcessedPage {
    page: CrawlPage,
    discovered: Vec<FrontierEntry>,
}

struct TaskFailure {
    entry: FrontierEntry,
    error: CrawlError,
    attempts: usize,
    redirect_chain: Vec<Url>,
    events: Vec<CrawlEvent>,
}

impl TaskFailure {
    fn into_model(self, redact_query_values: bool) -> CrawlFailure {
        let kind = match self.error {
            CrawlError::InvalidConfig(_) | CrawlError::InvalidUrl(_) => FailureKind::InvalidUrl,
            CrawlError::NetworkDenied(_) => FailureKind::NetworkDenied,
            CrawlError::Network(_) => FailureKind::Network,
            CrawlError::HttpStatus { .. } => FailureKind::HttpStatus,
            CrawlError::AttemptTimeout => FailureKind::Timeout,
            CrawlError::RobotsDenied(_) => FailureKind::RobotsDenied,
            CrawlError::RedirectDenied(_) => FailureKind::RedirectDenied,
            CrawlError::RedirectBudget => FailureKind::RedirectBudget,
            CrawlError::ResourceBudget { .. } => FailureKind::ResourceBudget,
            CrawlError::DeadlineExceeded => FailureKind::Deadline,
            CrawlError::Cancelled => FailureKind::Cancelled,
            CrawlError::Frontier(_) => FailureKind::Frontier,
            CrawlError::Analysis(_) => FailureKind::Analysis,
            CrawlError::Output(_) => FailureKind::Output,
        };
        let detail = CrawlFailureDetail {
            kind,
            message: self.error.to_string(),
            status: self.error.status(),
            attempts: self.attempts,
            redirect_chain: self
                .redirect_chain
                .iter()
                .map(|url| report_url(url, redact_query_values))
                .collect(),
            retryable: self.error.retryable(),
        };
        CrawlFailure {
            url: report_url(&self.entry.url, redact_query_values),
            depth: self.entry.depth,
            error: detail,
        }
    }
}

struct AttemptFailure {
    error: CrawlError,
    attempts: u8,
    events: Vec<CrawlEvent>,
}

enum PageTask {
    Success(Box<PageSuccess>),
    Failure(TaskFailure),
    Duplicate { events: Vec<CrawlEvent> },
}

impl PageTask {
    fn events(&self) -> &[CrawlEvent] {
        match self {
            Self::Success(success) => &success.events,
            Self::Failure(failure) => &failure.events,
            Self::Duplicate { events } => events,
        }
    }
}

struct RunOutput {
    report: CrawlReport,
    summary: CrawlSummary,
}

#[derive(Debug)]
struct ScopeContext {
    seed_host: String,
    seed_origin: String,
}

impl ScopeContext {
    fn new(seed: &Url) -> Result<Self> {
        Ok(Self {
            seed_host: seed
                .host_str()
                .ok_or_else(|| CrawlError::InvalidUrl("seed URL is missing a host".to_string()))?
                .to_ascii_lowercase(),
            seed_origin: origin_key(seed),
        })
    }
}

fn in_scope(config: &CrawlConfig, scope: &ScopeContext, url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let boundary_matches = match config.scope.boundary {
        ScopeBoundary::Origin => origin_key(url) == scope.seed_origin,
        ScopeBoundary::Domain => {
            host == scope.seed_host
                || (config.scope.allow_subdomains
                    && host.ends_with(&format!(".{}", scope.seed_host)))
        }
        ScopeBoundary::Any => true,
    };
    if !boundary_matches {
        return false;
    }
    let path = url.path();
    if config
        .scope
        .exclude_path_prefixes
        .iter()
        .any(|prefix| path_prefix_matches(path, prefix, config.scope.path_match_mode))
    {
        return false;
    }
    config.scope.include_path_prefixes.is_empty()
        || config
            .scope
            .include_path_prefixes
            .iter()
            .any(|prefix| path_prefix_matches(path, prefix, config.scope.path_match_mode))
}

fn path_prefix_matches(path: &str, prefix: &str, mode: PathMatchMode) -> bool {
    match mode {
        PathMatchMode::RawPrefix => path.starts_with(prefix),
        PathMatchMode::SegmentPrefix => {
            path == prefix
                || prefix == "/"
                || (path.starts_with(prefix)
                    && (prefix.ends_with('/') || path.as_bytes().get(prefix.len()) == Some(&b'/')))
        }
    }
}

fn path_and_query(url: &Url) -> String {
    url.query().map_or_else(
        || url.path().to_string(),
        |query| format!("{}?{query}", url.path()),
    )
}

fn retry_delay(
    config: &CrawlConfig,
    url: &Url,
    attempt: u8,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(delay) = retry_after {
        return delay.min(config.retry.max_delay);
    }
    let factor = 1_u32
        .checked_shl(u32::from(attempt.saturating_sub(1)))
        .unwrap_or(u32::MAX);
    let ceiling = config
        .retry
        .base_delay
        .saturating_mul(factor)
        .min(config.retry.max_delay);
    if ceiling.is_zero() {
        return Duration::ZERO;
    }
    // Deterministic full jitter avoids synchronized retry storms while keeping
    // tests reproducible for the same URL and attempt.
    let mut hasher = DefaultHasher::new();
    url.as_str().hash(&mut hasher);
    attempt.hash(&mut hasher);
    let ceiling_ms = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(hasher.finish() % ceiling_ms.saturating_add(1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitState {
    Continue,
    BrokenPipe,
}

fn emit_record(
    sink: &Arc<dyn CrawlSink>,
    budget: &CrawlBudget,
    report: &mut CrawlReport,
    record: CrawlRecord,
    collect: bool,
    collect_events: bool,
) -> Result<EmitState> {
    let mut counter = CountingWriter(0);
    serde_json::to_writer(&mut counter, &record)
        .map_err(|error| CrawlError::Output(error.to_string()))?;
    budget.reserve_report_bytes(counter.0.saturating_add(1))?;
    match sink.emit(&record) {
        Ok(()) => {}
        Err(CrawlSinkError::BrokenPipe) => return Ok(EmitState::BrokenPipe),
        Err(CrawlSinkError::Other(error)) => return Err(CrawlError::Output(error)),
    }
    if collect {
        match record {
            CrawlRecord::Event { value } if collect_events => report.events.push(value),
            CrawlRecord::Page { value } => report.pages.push(*value),
            CrawlRecord::Failure { value } => report.failures.push(value),
            CrawlRecord::Event { .. } | CrawlRecord::Summary { .. } => {}
        }
    }
    Ok(EmitState::Continue)
}

struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn report_url(url: &Url, redact_query_values: bool) -> Url {
    if redact_query_values {
        sanitized_url(url)
    } else {
        url.clone()
    }
}

fn sanitized_url(url: &Url) -> Url {
    Url::parse(&safe_url(url)).unwrap_or_else(|_| url.clone())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_path_prefix_has_a_boundary() {
        assert!(path_prefix_matches(
            "/docs/page",
            "/docs",
            PathMatchMode::SegmentPrefix
        ));
        assert!(!path_prefix_matches(
            "/docs-old",
            "/docs",
            PathMatchMode::SegmentPrefix
        ));
    }

    #[test]
    fn normalization_is_idempotent_and_fragment_insensitive() {
        let url = Url::parse("https://EXAMPLE.test/a#one").unwrap();
        let once = normalize_url(&url);
        let twice = normalize_url(&Url::parse(&once).unwrap());
        assert_eq!(once, twice);
        assert_eq!(
            once,
            normalize_url(&Url::parse("https://example.test/a#two").unwrap())
        );
    }
}

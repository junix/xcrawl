use std::collections::HashMap;
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
use crate::enumeration;
use crate::fetch::{
    Conditional, HopOutcome, HopResponse, OneHopTransport, origin_key, safe_url, validate_url,
};
use crate::frontier::{FrontierValidators, normalize_url};
use crate::robots::{RobotsRules, RobotsState};
use crate::throttle::OriginScheduler;
use crate::{
    AnalyzedLink, CrawlConfig, CrawlError, CrawlEvent, CrawlFailure, CrawlFailureDetail,
    CrawlOutcome, CrawlPage, CrawlRecord, CrawlReport, CrawlSink, CrawlSinkError, CrawlStats,
    CrawlSummary, EntryKind, FailureKind, Frontier, FrontierEntry, InMemoryFrontier, NullCrawlSink,
    PageAnalysis, PathMatchMode, RedirectPolicy, RequestKind, Result, ScopeBoundary,
};

/// The only non-2xx status an enumeration claim accepts: the server confirmed
/// a conditional request was unchanged.
const NOT_MODIFIED: u16 = 304;

#[derive(Clone)]
pub struct Crawler {
    config: Arc<CrawlConfig>,
    analyzer: Arc<dyn PageAnalyzer>,
    transport: OneHopTransport,
    analysis_worker: Option<std::path::PathBuf>,
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
            analysis_worker: None,
        })
    }

    /// Use a trusted xcrawl executable for cancellable, isolated parsing.
    #[must_use]
    pub fn with_analysis_worker(mut self, executable: std::path::PathBuf) -> Self {
        self.analysis_worker = Some(executable);
        self
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

    pub async fn crawl_with_frontier_and_sink(
        &self,
        seed: &Url,
        frontier: Arc<dyn Frontier>,
        sink: Arc<dyn CrawlSink>,
    ) -> Result<CrawlSummary> {
        Ok(self.run(seed, frontier, sink, false).await?.summary)
    }

    pub fn validate_seed(&self, seed: &Url) -> Result<()> {
        self.validate_seed_kind(seed, "seed")
    }

    fn validate_seed_kind(&self, seed: &Url, label: &str) -> Result<()> {
        validate_url(seed, &self.config.network)?;
        let scope = ScopeContext::new(seed)?;
        if !in_scope(&self.config, &scope, seed) {
            return Err(CrawlError::InvalidUrl(format!(
                "{label} URL is excluded by the configured scope policy"
            )));
        }
        if seed.as_str().len() > self.config.limits.max_url_length {
            return Err(CrawlError::ResourceBudget {
                resource: "url_length",
                limit: self.config.limits.max_url_length,
            });
        }
        Ok(())
    }

    /// Validate every designated enumeration seed exactly like the primary
    /// seed: same network, scope, and URL-length gates, measured against the
    /// primary seed's scope.
    pub fn validate_enumeration_seeds(&self, seed: &Url) -> Result<()> {
        let scope = ScopeContext::new(seed)?;
        for (url, label) in self.enumeration_seeds() {
            validate_url(&url, &self.config.network)?;
            if !in_scope(&self.config, &scope, &url) {
                return Err(CrawlError::InvalidUrl(format!(
                    "{label} URL is excluded by the configured scope policy"
                )));
            }
            if url.as_str().len() > self.config.limits.max_url_length {
                return Err(CrawlError::ResourceBudget {
                    resource: "url_length",
                    limit: self.config.limits.max_url_length,
                });
            }
        }
        Ok(())
    }

    fn enumeration_seeds(&self) -> Vec<(Url, &'static str)> {
        let mut seeds: Vec<(Url, &'static str)> = self
            .config
            .enumeration
            .sitemaps
            .iter()
            .map(|url| (url.clone(), "sitemap"))
            .chain(
                self.config
                    .enumeration
                    .feeds
                    .iter()
                    .map(|url| (url.clone(), "feed")),
            )
            .collect();
        seeds.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        seeds.dedup_by(|a, b| a.0.as_str() == b.0.as_str());
        seeds
    }

    /// The identity a durable frontier binds for this seed and policy. A
    /// directory shared by a different seed, enumeration set, or policy is
    /// rejected instead of silently merged.
    pub fn frontier_identity(&self, seed: &Url) -> String {
        let enumeration = self
            .enumeration_seeds()
            .into_iter()
            .map(|(url, label)| format!("{label}:{}", normalize_url(&url)))
            .collect::<Vec<_>>()
            .join("|");
        format!(
            "xcrawl:v1:{}:{:?}:{:?}:{}:{enumeration}",
            normalize_url(seed),
            self.config.scope,
            self.config.network,
            self.config.traversal.max_depth
        )
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
        self.validate_enumeration_seeds(seed)?;
        let _output_lease = self
            .config
            .output
            .private_directory
            .as_deref()
            .map(crate::resource::lock_output)
            .transpose()?;
        frontier
            .bind_identity(&self.frontier_identity(seed))
            .await?;
        let started = Instant::now();
        let scope = Arc::new(ScopeContext::new(seed)?);
        let budget = Arc::new(CrawlBudget::new(Arc::clone(&self.config))?);
        let runtime = Arc::new(CrawlRuntime {
            config: Arc::clone(&self.config),
            analyzer: Arc::clone(&self.analyzer),
            analysis_worker: self.analysis_worker.clone(),
            transport: self.transport.clone(),
            throttle: Arc::new(OriginScheduler::new(
                self.config.traversal.default_delay,
                self.config.traversal.max_origin_in_flight,
            )),
            budget: Arc::clone(&budget),
            robots: Arc::new(AsyncMutex::new(HashMap::new())),
            resources_seen: Arc::new(Mutex::new(HashMap::new())),
            frontier: Arc::clone(&frontier),
            scope,
        });

        // Enumeration seeds join the primary seed at depth zero; their
        // children flow through the same scope, robots, and budget gates.
        let mut initial = vec![FrontierEntry {
            url: seed.clone(),
            depth: 0,
            kind: EntryKind::Page,
        }];
        for (url, label) in self.enumeration_seeds() {
            initial.push(FrontierEntry {
                url,
                depth: 0,
                kind: match label {
                    "feed" => EntryKind::Feed,
                    _ => EntryKind::Sitemap,
                },
            });
        }
        let seed_result = frontier.enqueue_if_new(initial).await?;
        if seed_result.rejected_capacity > 0 {
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
            if budget.output_exhausted() {
                terminal_outcome = Some(CrawlOutcome::Partial);
                termination_reason = Some("output_limit".to_string());
                break;
            }
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
                let claimed =
                    tokio::time::timeout(budget.remaining_duration()?, frontier.pop()).await;
                let Some(entry) = (if let Ok(result) = claimed {
                    result?
                } else {
                    terminal_outcome = Some(CrawlOutcome::DeadlineExceeded);
                    termination_reason = Some("crawl deadline exceeded".to_string());
                    break;
                }) else {
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
                )
                .await?
                {
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
                        )
                        .await?
                            == EmitState::BrokenPipe
                        {
                            sink_closed = true;
                            terminal_outcome = Some(CrawlOutcome::Cancelled);
                            termination_reason =
                                Some("output consumer closed the stream".to_string());
                            break;
                        }
                    }
                    let page = processed.page;
                    report.stats.pages_fetched += 1;
                    if page.analysis_status == "analyzed" {
                        report.stats.pages_crawled += 1;
                        report.stats.pages_analyzed += 1;
                    } else if page.analysis_status.starts_with("enumerated") {
                        report.stats.pages_crawled += 1;
                        report.stats.pages_enumerated += 1;
                    } else {
                        report.stats.pages_skipped += 1;
                    }
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
                    )
                    .await?
                        == EmitState::BrokenPipe
                        || emit_record(
                            &sink,
                            &budget,
                            &mut report,
                            CrawlRecord::Page {
                                value: Box::new(page),
                            },
                            collect,
                            self.config.output.collect_events,
                        )
                        .await?
                            == EmitState::BrokenPipe
                    {
                        sink_closed = true;
                        terminal_outcome = Some(CrawlOutcome::Cancelled);
                        termination_reason = Some("output consumer closed the stream".to_string());
                    }
                }
                PageTask::Failure(failure) => {
                    let exhausted = match failure.error {
                        CrawlError::ResourceBudget {
                            resource: "download_bytes",
                            ..
                        } => Some("byte_limit"),
                        CrawlError::ResourceBudget {
                            resource: "http_requests",
                            ..
                        } => Some("request_limit"),
                        _ => None,
                    };
                    if let Some(reason) = exhausted {
                        terminal_outcome = Some(CrawlOutcome::Partial);
                        termination_reason = Some(reason.to_string());
                        report.stats.unfinished += 1;
                    } else {
                        frontier.complete(&failure.entry, Vec::new()).await?;
                    }
                    let failure = failure.into_model(self.config.output.redact_query_values);
                    report.stats.pages_failed = report.stats.pages_failed.saturating_add(1);
                    let event = CrawlEvent::Failed {
                        url: failure.url.clone(),
                        depth: failure.depth,
                        error: failure.error.clone(),
                    };
                    if failure.depth == 0 && exhausted.is_none() {
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
                    )
                    .await?
                        == EmitState::BrokenPipe
                        || emit_record(
                            &sink,
                            &budget,
                            &mut report,
                            CrawlRecord::Failure { value: failure },
                            collect,
                            self.config.output.collect_events,
                        )
                        .await?
                            == EmitState::BrokenPipe
                    {
                        sink_closed = true;
                        terminal_outcome = Some(CrawlOutcome::Cancelled);
                        termination_reason = Some("output consumer closed the stream".to_string());
                    }
                }
                PageTask::Duplicate { entry, .. } => {
                    frontier.complete(&entry, Vec::new()).await?;
                    report.stats.urls_filtered += 1;
                }
            }
        }

        if sink_closed && budget.remaining_duration().is_err() {
            terminal_outcome = Some(CrawlOutcome::DeadlineExceeded);
            termination_reason = Some("output_deadline".to_string());
        }
        if termination_reason.is_none() {
            if scheduled >= self.config.limits.max_pages && !frontier.is_empty().await? {
                termination_reason = Some("page_limit".to_string());
                terminal_outcome = Some(CrawlOutcome::Partial);
            } else if report.stats.frontier_rejected > 0 {
                termination_reason = Some("frontier_limit".to_string());
                terminal_outcome = Some(CrawlOutcome::Partial);
            } else {
                termination_reason = Some("frontier_exhausted".to_string());
            }
        }
        report.stats.unfinished += pending
            .len()
            .saturating_add(frontier.pending_count().await?);
        drop(pending);
        frontier.release_claims().await?;
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
        termination_reason = termination_reason.map(|reason| reason.chars().take(500).collect());
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
            )
            .await?;
            let _ = emit_record(
                &sink,
                &budget,
                &mut report,
                CrawlRecord::Summary {
                    value: summary.clone(),
                },
                false,
                false,
            )
            .await?;
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

        let mut candidates = Vec::new();
        if success.entry.depth < self.config.traversal.max_depth
            && !success.analysis.robots.nofollow
        {
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
                    kind: success.child_kind,
                });
            }
        }
        // Persist execution candidates before acknowledging the page. A crash
        // can replay discovery/submission, but cannot lose the acquisition.
        if let Some(directory) = &self.config.output.private_directory {
            for candidate in &success.analysis.resources {
                let path = directory
                    .join("outbox")
                    .join(format!("{}.json", candidate.resource_id));
                crate::resource::persist_candidate(&path, candidate)?;
            }
        }
        let result = frontier.complete(&success.entry, candidates).await?;
        stats.frontier_rejected = stats
            .frontier_rejected
            .saturating_add(result.rejected_capacity);
        stats.urls_filtered = stats
            .urls_filtered
            .saturating_add(result.duplicates)
            .saturating_add(result.rejected_capacity);
        success.enqueued = result.enqueued;

        let mut links = success.analysis.links;
        let links_truncated = links_discovered > self.config.traversal.max_links_to_report;
        links.truncate(self.config.traversal.max_links_to_report);
        if self.config.output.redact_query_values {
            for candidate in &mut success.analysis.resources {
                candidate.visibility = "public_redacted".to_string();
                candidate.url = sanitized_url(&candidate.url);
                candidate.source_page = sanitized_url(&candidate.source_page);
            }
            for link in &mut links {
                link.url = sanitized_url(&link.url);
            }
            if let Some(article) = &mut success.analysis.article {
                for value in [
                    &mut article.metadata.image,
                    &mut article.metadata.canonical_url,
                    &mut article.provenance.source_url,
                ]
                .into_iter()
                .flatten()
                {
                    *value = Url::parse(value)
                        .map_or_else(|_| "REDACTED".to_string(), |url| safe_url(&url));
                }
                for warning in &mut article.warnings {
                    warning.message = "analysis warning; see code".to_string();
                }
            }
            if let Some(error) = &mut success.analysis.article_error {
                error.message = "page analysis failed; see kind and stage".to_string();
            }
        }
        let enumeration_entry = success.entry.kind != EntryKind::Page;
        let decodable = crate::fetch::is_decodable_content_type(
            success.response.headers.content_type.as_deref(),
        );
        let page = CrawlPage {
            snapshot: self
                .config
                .output
                .private_directory
                .as_ref()
                .filter(|_| enumeration_entry || decodable)
                .map(|_| {
                    format!(
                        "snapshots/{}.body",
                        crate::resource::identity(&success.response.url, "snapshot")
                    )
                }),
            resources: success.analysis.resources,
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
            analysis_status: if enumeration_entry {
                match success.entry.kind {
                    EntryKind::Sitemap => "enumerated_sitemap",
                    EntryKind::Feed => "enumerated_feed",
                    EntryKind::Page => "analyzed",
                }
            } else if decodable {
                "analyzed"
            } else {
                "skipped_unsupported_content_type"
            }
            .to_string(),
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
    analysis_worker: Option<std::path::PathBuf>,
    transport: OneHopTransport,
    throttle: Arc<OriginScheduler>,
    budget: Arc<CrawlBudget>,
    robots: Arc<AsyncMutex<HashMap<String, Arc<OnceCell<CachedRobots>>>>>,
    resources_seen: Arc<Mutex<HashMap<String, usize>>>,
    frontier: Arc<dyn Frontier>,
    scope: Arc<ScopeContext>,
}

impl CrawlRuntime {
    async fn fetch_and_analyze(&self, entry: FrontierEntry) -> PageTask {
        let request_key = normalize_url(&entry.url);
        if self
            .resources_seen
            .lock()
            .expect("resource seen lock poisoned")
            .get(&request_key)
            .is_some_and(|depth| *depth <= entry.depth)
        {
            return PageTask::Duplicate {
                entry,
                events: Vec::new(),
            };
        }
        let conditional = if entry.kind == EntryKind::Page {
            None
        } else {
            match self.frontier.validators(&entry.url).await {
                Ok(validators) => validators,
                Err(error) => {
                    return PageTask::Failure(TaskFailure {
                        entry,
                        error,
                        attempts: 0,
                        redirect_chain: Vec::new(),
                        events: Vec::new(),
                    });
                }
            }
        };
        let conditional_headers = conditional.as_ref().map(|validators| Conditional {
            if_none_match: validators.etag.as_deref(),
            if_modified_since: validators.last_modified.as_deref(),
        });
        let fetched = match self
            .fetch_page(
                &entry,
                enumeration::request_kind(entry.kind),
                conditional_headers,
            )
            .await
        {
            Ok(fetched) => fetched,
            Err(failure) => return PageTask::Failure(failure),
        };
        if entry.kind != EntryKind::Page {
            return self.enumeration_task(entry, fetched).await;
        }
        let resource_key = normalize_url(&fetched.response.url);
        let duplicate = {
            let mut seen = self
                .resources_seen
                .lock()
                .expect("resource seen lock poisoned");
            if seen
                .get(&resource_key)
                .is_some_and(|depth| *depth <= entry.depth)
            {
                true
            } else {
                seen.insert(resource_key, entry.depth);
                false
            }
        };
        if duplicate {
            return PageTask::Duplicate {
                entry,
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
        if let Some(directory) =
            self.config.output.private_directory.as_ref().filter(|_| {
                crate::fetch::is_decodable_content_type(headers.content_type.as_deref())
            })
        {
            let snapshot_id = crate::resource::identity(&url, "snapshot");
            if let Err(error) = crate::resource::atomic_private(
                &directory
                    .join("snapshots")
                    .join(format!("{snapshot_id}.body")),
                &body,
            ) {
                return PageTask::Failure(TaskFailure {
                    entry,
                    error,
                    attempts: fetched.attempts,
                    redirect_chain: fetched.redirect_chain,
                    events: fetched.events,
                });
            }
        }
        let input = PageInput {
            final_url: url.clone(),
            content_type: headers.content_type.clone(),
            body,
            response_headers: headers.values.clone(),
            max_links: self.config.traversal.max_links_to_analyze,
        };
        let analyzer = Arc::clone(&self.analyzer);
        let result = if crate::fetch::is_decodable_content_type(input.content_type.as_deref()) {
            crate::analyzer::analyze_bounded(analyzer, input, self.analysis_worker.as_deref()).await
        } else {
            Ok(PageAnalysis {
                resources: crate::resource::from_response(
                    &entry.url,
                    &url,
                    headers.content_type.as_deref(),
                ),
                detected_encoding: "binary".to_string(),
                ..PageAnalysis::default()
            })
        };
        let analysis = match result {
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
            child_kind: EntryKind::Page,
        }))
    }

    /// Expand a designated sitemap or feed document. The raw body (including
    /// gzip-wrapped files) is snapshotted and its validators persisted before
    /// expansion, so a crash resumes with a conditional request and can
    /// re-expand from the snapshot on 304 without re-downloading anything.
    async fn enumeration_task(&self, entry: FrontierEntry, fetched: Fetched) -> PageTask {
        let kind = entry.kind;
        let mut fetched = fetched;
        if fetched.response.status == NOT_MODIFIED
            && let Err(error) = self.reuse_snapshot(&mut fetched)
        {
            // Without the retained body the document cannot be re-expanded;
            // fall back to exactly one unconditional request.
            tracing::debug!(url = %safe_url(&entry.url), %error, "conditional refetch");
            match self
                .fetch_page(&entry, RequestKind::Enumeration, None)
                .await
            {
                Ok(refetched) => fetched = refetched,
                Err(failure) => return PageTask::Failure(failure),
            }
        }
        let body = std::mem::take(&mut fetched.response.body);
        let body_bytes = body.len();
        let validators = FrontierValidators {
            etag: fetched.response.headers.etag.clone(),
            last_modified: fetched.response.headers.last_modified.clone(),
        };
        // Persist the raw body and validators before expansion: a crash here
        // resumes with a conditional request and the retained snapshot.
        if let Some(directory) = self.config.output.private_directory.as_ref() {
            let snapshot_id = crate::resource::identity(&fetched.response.url, "snapshot");
            if let Err(error) = crate::resource::atomic_private(
                &directory
                    .join("snapshots")
                    .join(format!("{snapshot_id}.body")),
                &body,
            ) {
                return PageTask::Failure(TaskFailure {
                    entry,
                    error,
                    attempts: fetched.attempts,
                    redirect_chain: fetched.redirect_chain,
                    events: fetched.events,
                });
            }
            if !validators.is_empty()
                && let Err(error) = self
                    .frontier
                    .store_validators(&entry.url, &validators)
                    .await
            {
                return PageTask::Failure(TaskFailure {
                    entry,
                    error,
                    attempts: fetched.attempts,
                    redirect_chain: fetched.redirect_chain,
                    events: fetched.events,
                });
            }
        }
        let document = match enumeration::parse_document(
            kind,
            &fetched.response.url,
            &body,
            self.config.traversal.max_links_to_analyze,
        ) {
            Ok(document) => document,
            Err(error) => {
                return PageTask::Failure(TaskFailure {
                    entry,
                    error,
                    attempts: fetched.attempts,
                    redirect_chain: fetched.redirect_chain,
                    events: fetched.events,
                });
            }
        };
        let child_kind = document
            .children
            .first()
            .map_or(EntryKind::Page, |child| child.kind);
        let links_discovered = document.total_discovered;
        let links = document
            .children
            .into_iter()
            .map(|child| AnalyzedLink {
                url: child.url,
                text: child.title,
                rel: Vec::new(),
                nofollow: false,
            })
            .collect();
        let analysis = PageAnalysis {
            links,
            links_discovered,
            detected_encoding: "utf-8".to_string(),
            ..PageAnalysis::default()
        };
        PageTask::Success(Box::new(PageSuccess {
            entry,
            response: HopResponse {
                body: Vec::new(),
                ..fetched.response
            },
            body_bytes,
            redirect_chain: fetched.redirect_chain,
            events: fetched.events,
            analysis,
            scope: Arc::clone(&self.scope),
            enqueued: Vec::new(),
            child_kind,
        }))
    }

    /// Load the retained snapshot for a 304 response. The server confirmed the
    /// document is unchanged, so the stored bytes are exactly what a 200 would
    /// have returned.
    fn reuse_snapshot(&self, fetched: &mut Fetched) -> Result<()> {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let Some(directory) = self.config.output.private_directory.as_ref() else {
            return Err(CrawlError::Output(
                "no private output directory retained the document".to_string(),
            ));
        };
        let path = directory.join("snapshots").join(format!(
            "{}.body",
            crate::resource::identity(&fetched.response.url, "snapshot")
        ));
        let file = std::fs::File::from(
            rustix::fs::open(
                &path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            )
            .map_err(|error| {
                CrawlError::Output(format!(
                    "snapshot for an unchanged document is unavailable: {error}"
                ))
            })?,
        );
        let metadata = file
            .metadata()
            .map_err(|error| CrawlError::Output(error.to_string()))?;
        if !metadata.is_file() || metadata.size() > self.config.limits.max_response_bytes as u64 {
            return Err(CrawlError::Output(
                "snapshot for an unchanged document is missing or oversized".to_string(),
            ));
        }
        let mut body = Vec::new();
        file.take(self.config.limits.max_response_bytes as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|error| CrawlError::Output(error.to_string()))?;
        if body.len() > self.config.limits.max_response_bytes {
            return Err(CrawlError::Output(
                "snapshot for an unchanged document exceeds the response limit".to_string(),
            ));
        }
        fetched.response.body = body;
        Ok(())
    }

    #[allow(clippy::result_large_err)] // Error retains bounded request provenance for the report.
    async fn fetch_page(
        &self,
        entry: &FrontierEntry,
        kind: RequestKind,
        conditional: Option<Conditional<'_>>,
    ) -> std::result::Result<Fetched, TaskFailure> {
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
                    kind,
                    self.config.limits.max_response_bytes,
                    conditional,
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
                // A conditional enumeration request the server answered with
                // "not modified": the retained snapshot stands in for the body.
                HopOutcome::Response(response)
                    if response.status == NOT_MODIFIED
                        && kind == RequestKind::Enumeration
                        && conditional.is_some() =>
                {
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
                            error: CrawlError::RedirectBudget {
                                limit: self.config.scope.max_redirects,
                            },
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
            RedirectPolicy::SameOrigin if origin_key(url) != initial_origin => {
                Err(CrawlError::RedirectDenied(format!(
                    "cross-origin redirect to {} is not followed automatically; retry against that URL directly",
                    origin_key(url)
                )))
            }
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
                    None,
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
                HopOutcome::Response(response)
                    if (400..500).contains(&response.status) && response.status != 429 =>
                {
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
                            error: CrawlError::RedirectBudget {
                                limit: self.config.robots.max_redirects,
                            }
                            .to_string(),
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
        conditional: Option<Conditional<'_>>,
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
                    .send_one_hop(url, kind, max_body_bytes, &self.budget, conditional),
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
    /// The frontier kind this document's discovered links enqueue as. A
    /// sitemap index discovers further sitemaps; everything else discovers
    /// pages.
    child_kind: EntryKind,
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
            CrawlError::RedirectBudget { .. } => FailureKind::RedirectBudget,
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
    Duplicate {
        entry: FrontierEntry,
        events: Vec<CrawlEvent>,
    },
}

impl PageTask {
    fn events(&self) -> &[CrawlEvent] {
        match self {
            Self::Success(success) => &success.events,
            Self::Failure(failure) => &failure.events,
            Self::Duplicate { events, .. } => events,
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
        return delay;
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

async fn emit_record(
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
    // Terminal diagnostics have a separate, bounded allowance so normal
    // output exhaustion never discards the report or suppresses its summary.
    let terminal = matches!(
        &record,
        CrawlRecord::Summary { .. }
            | CrawlRecord::Event {
                value: CrawlEvent::Complete { .. }
            }
    );
    if !terminal
        && (budget.output_exhausted()
            || budget
                .reserve_report_bytes(
                    if collect && matches!(&record, CrawlRecord::Event { .. }) && !collect_events {
                        0
                    } else {
                        counter.0.saturating_add(1)
                    },
                    collect,
                )
                .is_err())
    {
        return Ok(EmitState::Continue);
    }
    let allowance = if terminal {
        Duration::from_millis(100)
    } else {
        budget.remaining_duration().unwrap_or(Duration::ZERO)
    };
    match tokio::time::timeout(allowance, sink.emit(&record)).await {
        Ok(Ok(())) => {}
        Err(_) | Ok(Err(CrawlSinkError::BrokenPipe)) => return Ok(EmitState::BrokenPipe),
        Ok(Err(CrawlSinkError::Other(error))) => return Err(CrawlError::Output(error)),
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
#[path = "crawler_tests.rs"]
mod tests;

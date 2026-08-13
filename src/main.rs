use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{ArgAction, Parser, ValueEnum};
use serde::Serialize;
use serde_json::json;
use tracing_subscriber::EnvFilter;
use url::Url;
use xcrawl::{
    CrawlConfig, CrawlError, CrawlOutcome, CrawlRecord, CrawlSink, CrawlSinkError, CrawlStrategy,
    Crawler, PathMatchMode, PortPolicy, RedirectPolicy, ScopeBoundary,
};

const LONG_HELP: &str = concat!(
    "Examples:\n",
    "  xcrawl https://example.com --max-pages 25 --max-depth 2\n",
    "  xcrawl https://example.com --include-path-prefix /docs --exclude-path-prefix /private\n",
    "  xcrawl https://example.com --delay 500ms --timeout 20s --max-response-bytes 4MiB\n",
    "  xcrawl https://example.com --format json --compact\n\n",
    "The default output is streaming JSON Lines. Durations accept ms, s, or m.\n",
    "Byte sizes accept B, KB, KiB, MB, MiB, GB, or GiB.\n",
    "Exit codes: 0 complete/allowed partial, 1 partial/fatal, 2 usage, 3 invalid policy, 4 seed/network failure."
);

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Policy-enforced bounded native web crawler",
    next_line_help = true,
    after_long_help = LONG_HELP
)]
struct Cli {
    /// Absolute HTTP(S) seed URL.
    url: Url,

    /// Maximum logical pages scheduled.
    #[arg(long, default_value_t = 100, help_heading = "Traversal")]
    max_pages: usize,

    /// Maximum link depth from the seed; zero fetches only the seed.
    #[arg(long, default_value_t = 2, help_heading = "Traversal")]
    max_depth: usize,

    /// Maximum logical page tasks in flight.
    #[arg(long, default_value_t = 8, help_heading = "Traversal")]
    concurrency: usize,

    /// Maximum simultaneous requests to one exact origin.
    #[arg(long, default_value_t = 1, help_heading = "Traversal")]
    max_origin_in_flight: usize,

    /// Maximum analyzed links retained from a page.
    #[arg(long, default_value_t = 2_000, help_heading = "Traversal")]
    max_links_to_analyze: usize,

    /// Maximum links considered for enqueueing from a page.
    #[arg(
        long,
        visible_alias = "max-links-per-page",
        default_value_t = 1_000,
        help_heading = "Traversal"
    )]
    max_links_to_enqueue: usize,

    /// Maximum links stored in one page output record.
    #[arg(long, default_value_t = 1_000, help_heading = "Traversal")]
    max_links_to_report: usize,

    /// Frontier ordering policy.
    #[arg(long, value_enum, default_value_t = StrategyArg::Bfs, help_heading = "Traversal")]
    strategy: StrategyArg,

    /// Permit traversal across arbitrary origins.
    #[arg(long, help_heading = "Scope")]
    allow_cross_domain: bool,

    /// Permit same-domain scheme/port changes and optional subdomains.
    #[arg(long, help_heading = "Scope")]
    domain_scope: bool,

    /// Permit subdomains when domain scope is selected.
    #[arg(long, help_heading = "Scope")]
    allow_subdomains: bool,

    /// Follow links marked rel=nofollow.
    #[arg(long, help_heading = "Scope")]
    follow_nofollow: bool,

    /// Follow only paths beginning with this segment prefix; repeatable.
    #[arg(long, value_name = "PATH", action = ArgAction::Append, help_heading = "Scope")]
    include_path_prefix: Vec<String>,

    /// Reject paths beginning with this segment prefix; repeatable.
    #[arg(long, value_name = "PATH", action = ArgAction::Append, help_heading = "Scope")]
    exclude_path_prefix: Vec<String>,

    /// Use literal string prefixes instead of segment-aware path prefixes.
    #[arg(long, help_heading = "Scope")]
    raw_path_prefix: bool,

    /// Redirect scope policy.
    #[arg(long, value_enum, default_value_t = RedirectArg::Scope, help_heading = "Scope")]
    redirect_policy: RedirectArg,

    /// Permit redirects from HTTPS to HTTP.
    #[arg(long, help_heading = "Scope")]
    allow_https_downgrade: bool,

    /// Maximum redirect hops for pages and robots.txt.
    #[arg(long, default_value_t = 5, help_heading = "Scope")]
    max_redirects: u8,

    /// Do not fetch or enforce robots.txt.
    #[arg(long, help_heading = "Robots")]
    ignore_robots: bool,

    /// Maximum accepted Crawl-delay/request-rate interval.
    #[arg(long, default_value = "60s", value_parser = parse_duration, help_heading = "Robots")]
    max_robots_delay: Duration,

    /// Maximum robots.txt response size (minimum 500 KiB when enforced).
    #[arg(long, default_value = "512KiB", value_parser = parse_byte_size, help_heading = "Robots")]
    max_robots_bytes: usize,

    /// Maximum robots.txt redirects (RFC 9309 requires at least five).
    #[arg(long, default_value_t = 5, help_heading = "Robots")]
    max_robots_redirects: u8,

    /// Minimum interval between starts of requests to one origin.
    #[arg(long, default_value = "250ms", value_parser = parse_duration, help_heading = "Politeness")]
    delay: Duration,

    /// Timeout for each HTTP attempt, including connect and body streaming.
    #[arg(long, default_value = "30s", value_parser = parse_duration, help_heading = "Network")]
    timeout: Duration,

    /// Timeout for one DNS resolution.
    #[arg(long, default_value = "5s", value_parser = parse_duration, help_heading = "Network")]
    dns_timeout: Duration,

    /// Maximum response body retained for one page.
    #[arg(
        long,
        visible_alias = "max-download-bytes",
        default_value = "8MiB",
        value_parser = parse_byte_size,
        help_heading = "Network"
    )]
    max_response_bytes: usize,

    /// Permit IANA non-global and caller-denied address ranges.
    #[arg(long, help_heading = "Network")]
    allow_private_networks: bool,

    /// Permit TCP ports other than 80 and 443.
    #[arg(long, help_heading = "Network")]
    allow_nonstandard_ports: bool,

    /// Value sent in User-Agent and used for robots group matching.
    #[arg(long, default_value_t = default_user_agent(), help_heading = "Network")]
    user_agent: String,

    /// Maximum retries after the initial attempt (0-9).
    #[arg(long, default_value_t = 2, help_heading = "Retry")]
    max_retries: u8,

    /// Initial exponential-backoff ceiling.
    #[arg(long, default_value = "100ms", value_parser = parse_duration, help_heading = "Retry")]
    retry_base_delay: Duration,

    /// Maximum retry and Retry-After delay.
    #[arg(long, default_value = "5s", value_parser = parse_duration, help_heading = "Retry")]
    retry_max_delay: Duration,

    /// Ignore Retry-After response headers.
    #[arg(long, help_heading = "Retry")]
    ignore_retry_after: bool,

    /// Maximum actual HTTP attempts, including robots, retries, and redirects.
    #[arg(long, default_value_t = 1_000, help_heading = "Limits")]
    max_http_requests: usize,

    /// Maximum bytes downloaded across the crawl.
    #[arg(long, default_value = "128MiB", value_parser = parse_byte_size, help_heading = "Limits")]
    max_total_download_bytes: usize,

    /// Maximum distinct origins contacted.
    #[arg(long, default_value_t = 32, help_heading = "Limits")]
    max_unique_origins: usize,

    /// Maximum distinct entries admitted to the frontier.
    #[arg(long, default_value_t = 10_000, help_heading = "Limits")]
    max_frontier_entries: usize,

    /// Maximum URL length in bytes.
    #[arg(long, default_value_t = 8_192, help_heading = "Limits")]
    max_url_length: usize,

    /// Overall crawl deadline.
    #[arg(long, default_value = "10m", value_parser = parse_duration, help_heading = "Limits")]
    max_crawl_duration: Duration,

    /// Maximum bytes in a collected JSON report.
    #[arg(long, default_value = "64MiB", value_parser = parse_byte_size, help_heading = "Limits")]
    max_report_bytes: usize,

    /// Validate and print the effective plan without network access.
    #[arg(long, help_heading = "Output")]
    dry_run: bool,

    /// Output format; JSON Lines streams by default.
    #[arg(long, value_enum, default_value_t = FormatArg::Jsonl, help_heading = "Output")]
    format: FormatArg,

    /// Emit single-line JSON in collected JSON mode.
    #[arg(long, help_heading = "Output")]
    compact: bool,

    /// Include query values in reports; values are redacted by default.
    #[arg(long, help_heading = "Output")]
    include_query_values: bool,

    /// Allow a partial crawl to exit successfully.
    #[arg(long, help_heading = "Outcome")]
    allow_partial: bool,

    /// Make any page failure exit nonzero (the default unless allow-partial is set).
    #[arg(long, help_heading = "Outcome")]
    fail_on_any_error: bool,

    /// Increase stderr diagnostics; repeat for debug-level logging.
    #[arg(short, long, action = ArgAction::Count, help_heading = "Diagnostics")]
    verbose: u8,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StrategyArg {
    Bfs,
    Dfs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RedirectArg {
    Scope,
    SameOrigin,
    Any,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FormatArg {
    Jsonl,
    Json,
}

impl Cli {
    fn crawl_config(&self) -> Result<CrawlConfig, CrawlError> {
        if self.allow_cross_domain && self.domain_scope {
            return Err(CrawlError::InvalidConfig(
                "--allow-cross-domain conflicts with --domain-scope".to_string(),
            ));
        }
        if self.allow_subdomains && !self.domain_scope {
            return Err(CrawlError::InvalidConfig(
                "--allow-subdomains requires --domain-scope".to_string(),
            ));
        }
        if self.allow_partial && self.fail_on_any_error {
            return Err(CrawlError::InvalidConfig(
                "--allow-partial conflicts with --fail-on-any-error".to_string(),
            ));
        }
        let max_attempts = self
            .max_retries
            .checked_add(1)
            .ok_or_else(|| CrawlError::InvalidConfig("max_retries is too large".to_string()))?;
        let mut config = CrawlConfig::default();
        config.traversal.max_depth = self.max_depth;
        config.traversal.concurrency = self.concurrency;
        config.traversal.max_origin_in_flight = self.max_origin_in_flight;
        config.traversal.max_links_to_analyze = self.max_links_to_analyze;
        config.traversal.max_links_to_enqueue = self.max_links_to_enqueue;
        config.traversal.max_links_to_report = self.max_links_to_report;
        config.traversal.strategy = match self.strategy {
            StrategyArg::Bfs => CrawlStrategy::BreadthFirst,
            StrategyArg::Dfs => CrawlStrategy::DepthFirst,
        };
        config.traversal.follow_nofollow = self.follow_nofollow;
        config.traversal.default_delay = self.delay;

        config.scope.boundary = if self.allow_cross_domain {
            ScopeBoundary::Any
        } else if self.domain_scope {
            ScopeBoundary::Domain
        } else {
            ScopeBoundary::Origin
        };
        config.scope.allow_subdomains = self.allow_subdomains;
        config
            .scope
            .include_path_prefixes
            .clone_from(&self.include_path_prefix);
        config
            .scope
            .exclude_path_prefixes
            .clone_from(&self.exclude_path_prefix);
        config.scope.path_match_mode = if self.raw_path_prefix {
            PathMatchMode::RawPrefix
        } else {
            PathMatchMode::SegmentPrefix
        };
        config.scope.redirect_policy = match self.redirect_policy {
            RedirectArg::Scope => RedirectPolicy::WithinCrawlScope,
            RedirectArg::SameOrigin => RedirectPolicy::SameOrigin,
            RedirectArg::Any => RedirectPolicy::Any,
        };
        config.scope.allow_https_downgrade = self.allow_https_downgrade;
        config.scope.max_redirects = self.max_redirects;

        config.robots.respect = !self.ignore_robots;
        config.robots.max_delay = self.max_robots_delay;
        config.robots.max_redirects = self.max_robots_redirects;
        config.network.deny_non_global = !self.allow_private_networks;
        config.network.allowed_ports = if self.allow_nonstandard_ports {
            PortPolicy::Any
        } else {
            PortPolicy::WebOnly
        };
        config.network.dns_timeout = self.dns_timeout;
        config.network.user_agent.clone_from(&self.user_agent);
        config.retry.max_attempts = max_attempts;
        config.retry.base_delay = self.retry_base_delay;
        config.retry.max_delay = self.retry_max_delay;
        config.retry.honor_retry_after = !self.ignore_retry_after;

        config.limits.max_pages = self.max_pages;
        config.limits.max_http_requests = self.max_http_requests;
        config.limits.max_total_download_bytes = self.max_total_download_bytes;
        config.limits.max_unique_origins = self.max_unique_origins;
        config.limits.max_frontier_entries = self.max_frontier_entries;
        config.limits.max_url_length = self.max_url_length;
        config.limits.max_crawl_duration = self.max_crawl_duration;
        config.limits.max_attempt_duration = self.timeout;
        config.limits.max_response_bytes = self.max_response_bytes;
        config.limits.max_robots_bytes = self.max_robots_bytes;
        config.limits.max_report_bytes = self.max_report_bytes;
        config.output.collect_events = matches!(self.format, FormatArg::Json);
        config.output.redact_query_values = !self.include_query_values;
        config.validate()?;
        Ok(config)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let config = match cli.crawl_config() {
        Ok(config) => config,
        Err(error) => return report_crawl_error(&error),
    };
    let crawler = match Crawler::new(config.clone()) {
        Ok(crawler) => crawler,
        Err(error) => return report_crawl_error(&error),
    };
    if let Err(error) = crawler.validate_seed(&cli.url) {
        return report_crawl_error(&error);
    }
    if cli.dry_run {
        return emit_json(&crawl_plan(&cli.url, &config), cli.compact);
    }

    match cli.format {
        FormatArg::Json => match crawler.crawl(&cli.url).await {
            Ok(report) => {
                let output = emit_json(&report, cli.compact);
                if output == ExitCode::SUCCESS {
                    outcome_exit(report.outcome, cli.allow_partial, cli.fail_on_any_error)
                } else {
                    output
                }
            }
            Err(error) => report_crawl_error(&error),
        },
        FormatArg::Jsonl => {
            let sink: Arc<dyn CrawlSink> = Arc::new(JsonLinesSink::default());
            match crawler.crawl_with_sink(&cli.url, sink).await {
                Ok(summary) => {
                    outcome_exit(summary.outcome, cli.allow_partial, cli.fail_on_any_error)
                }
                Err(CrawlError::Cancelled) => ExitCode::SUCCESS,
                Err(error) => report_crawl_error(&error),
            }
        }
    }
}

#[derive(Debug)]
struct JsonLinesSink {
    stdout: Mutex<io::Stdout>,
}

impl Default for JsonLinesSink {
    fn default() -> Self {
        Self {
            stdout: Mutex::new(io::stdout()),
        }
    }
}

impl CrawlSink for JsonLinesSink {
    fn emit(&self, record: &CrawlRecord) -> std::result::Result<(), CrawlSinkError> {
        let mut stdout = self
            .stdout
            .lock()
            .map_err(|error| CrawlSinkError::Other(error.to_string()))?;
        serde_json::to_writer(&mut *stdout, record).map_err(|error| {
            if error.io_error_kind() == Some(io::ErrorKind::BrokenPipe) {
                CrawlSinkError::BrokenPipe
            } else {
                CrawlSinkError::Other(error.to_string())
            }
        })?;
        writeln!(stdout).map_err(|error| {
            if error.kind() == io::ErrorKind::BrokenPipe {
                CrawlSinkError::BrokenPipe
            } else {
                CrawlSinkError::Other(error.to_string())
            }
        })?;
        stdout.flush().map_err(|error| {
            if error.kind() == io::ErrorKind::BrokenPipe {
                CrawlSinkError::BrokenPipe
            } else {
                CrawlSinkError::Other(error.to_string())
            }
        })
    }
}

fn init_tracing(verbosity: u8) {
    let fallback = match verbosity {
        0 => "xcrawl=warn",
        1 => "xcrawl=info",
        _ => "xcrawl=debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn crawl_plan(url: &Url, config: &CrawlConfig) -> serde_json::Value {
    json!({
        "schema_version": 2,
        "dry_run": true,
        "seed_url": url,
        "config": {
            "traversal": {
                "max_depth": config.traversal.max_depth,
                "concurrency": config.traversal.concurrency,
                "max_origin_in_flight": config.traversal.max_origin_in_flight,
                "max_links_to_analyze": config.traversal.max_links_to_analyze,
                "max_links_to_enqueue": config.traversal.max_links_to_enqueue,
                "max_links_to_report": config.traversal.max_links_to_report,
                "strategy": config.traversal.strategy,
                "default_delay_ms": duration_millis(config.traversal.default_delay),
            },
            "scope": {
                "boundary": config.scope.boundary,
                "allow_subdomains": config.scope.allow_subdomains,
                "include_path_prefixes": config.scope.include_path_prefixes,
                "exclude_path_prefixes": config.scope.exclude_path_prefixes,
                "path_match_mode": config.scope.path_match_mode,
                "redirect_policy": config.scope.redirect_policy,
                "allow_https_downgrade": config.scope.allow_https_downgrade,
                "max_redirects": config.scope.max_redirects,
            },
            "robots": {
                "respect": config.robots.respect,
                "max_delay_ms": duration_millis(config.robots.max_delay),
                "max_redirects": config.robots.max_redirects,
            },
            "retry": {
                "max_attempts": config.retry.max_attempts,
                "base_delay_ms": duration_millis(config.retry.base_delay),
                "max_delay_ms": duration_millis(config.retry.max_delay),
                "honor_retry_after": config.retry.honor_retry_after,
            },
            "limits": {
                "max_pages": config.limits.max_pages,
                "max_http_requests": config.limits.max_http_requests,
                "max_total_download_bytes": config.limits.max_total_download_bytes,
                "max_unique_origins": config.limits.max_unique_origins,
                "max_frontier_entries": config.limits.max_frontier_entries,
                "max_url_length": config.limits.max_url_length,
                "max_crawl_duration_ms": duration_millis(config.limits.max_crawl_duration),
                "max_attempt_duration_ms": duration_millis(config.limits.max_attempt_duration),
                "max_response_bytes": config.limits.max_response_bytes,
                "max_robots_bytes": config.limits.max_robots_bytes,
                "max_report_bytes": config.limits.max_report_bytes,
            },
            "network": {
                "deny_non_global": config.network.deny_non_global,
                "dns_timeout_ms": duration_millis(config.network.dns_timeout),
                "user_agent": config.network.user_agent,
            },
            "output": {
                "redact_query_values": config.output.redact_query_values,
            }
        }
    })
}

fn outcome_exit(outcome: CrawlOutcome, allow_partial: bool, _fail_on_any_error: bool) -> ExitCode {
    match outcome {
        CrawlOutcome::Complete | CrawlOutcome::Cancelled => ExitCode::SUCCESS,
        CrawlOutcome::Partial if allow_partial => ExitCode::SUCCESS,
        CrawlOutcome::SeedFailed => ExitCode::from(4),
        CrawlOutcome::Partial | CrawlOutcome::DeadlineExceeded | CrawlOutcome::Fatal => {
            ExitCode::FAILURE
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn emit_json(value: &impl Serialize, compact: bool) -> ExitCode {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let result = if compact {
        serde_json::to_writer(&mut stdout, value)
    } else {
        serde_json::to_writer_pretty(&mut stdout, value)
    };
    if let Err(error) = result {
        if error.io_error_kind() == Some(io::ErrorKind::BrokenPipe) {
            return ExitCode::SUCCESS;
        }
        eprintln!("failed to serialize JSON output: {error}");
        return ExitCode::FAILURE;
    }
    match writeln!(stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("failed to write JSON output: {error}");
            ExitCode::FAILURE
        }
    }
}

fn report_crawl_error(error: &CrawlError) -> ExitCode {
    eprintln!("error: {error}");
    match error {
        CrawlError::InvalidConfig(_) | CrawlError::InvalidUrl(_) => ExitCode::from(3),
        CrawlError::NetworkDenied(_) | CrawlError::Network(_) | CrawlError::AttemptTimeout => {
            ExitCode::from(4)
        }
        CrawlError::Cancelled => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(value) = raw.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = raw.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = raw.strip_suffix('m') {
        (value, 60_000)
    } else {
        return Err("expected a duration ending in ms, s, or m (for example 250ms or 30s)".into());
    };
    let value = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration value: {raw}"))?;
    let millis = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration is too large: {raw}"))?;
    Ok(Duration::from_millis(millis))
}

fn parse_byte_size(raw: &str) -> Result<usize, String> {
    let lowercase = raw.to_ascii_lowercase();
    let units = [
        ("gib", 1_073_741_824_u64),
        ("gb", 1_000_000_000),
        ("mib", 1_048_576),
        ("mb", 1_000_000),
        ("kib", 1_024),
        ("kb", 1_000),
        ("b", 1),
    ];
    let (number, multiplier) = units
        .iter()
        .find_map(|(suffix, multiplier)| {
            lowercase
                .strip_suffix(suffix)
                .map(|number| (number, *multiplier))
        })
        .unwrap_or((lowercase.as_str(), 1));
    let value = number
        .parse::<u64>()
        .map_err(|_| format!("invalid byte size: {raw}"))?;
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte size is too large: {raw}"))?;
    usize::try_from(bytes).map_err(|_| format!("byte size is too large: {raw}"))
}

fn default_user_agent() -> String {
    format!("xcrawl/{}", xcrawl::VERSION)
}

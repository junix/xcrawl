use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgAction, Parser, ValueEnum};
use serde::Serialize;
use serde_json::json;
use tracing_subscriber::EnvFilter;
use url::Url;
use xcrawl::{CrawlConfig, CrawlError, CrawlStrategy, Crawler};

const LONG_HELP: &str = concat!(
    "Examples:\n",
    "  xcrawl https://example.com --max-pages 25 --max-depth 2\n",
    "  xcrawl https://example.com --include-path-prefix /docs --exclude-path-prefix /private\n",
    "  xcrawl https://example.com --delay 500ms --timeout 20s --max-download-bytes 4MiB\n",
    "  xcrawl https://example.com --dry-run --compact\n\n",
    "Durations accept ms, s, or m. Byte sizes accept B, KB, KiB, MB, MiB, GB, or GiB.\n",
    "All result data is written to stdout; diagnostics are written to stderr.\n",
    "Exit codes: 0 success, 1 crawl/output failure, 2 usage error, 3 invalid policy, 4 denied/unreachable network.\n",
    "Local source: ",
    env!("CARGO_MANIFEST_DIR")
);

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Bounded native web crawler",
    next_line_help = true,
    after_long_help = LONG_HELP
)]
struct Cli {
    /// Absolute HTTP(S) seed URL.
    url: Url,

    /// Maximum number of pages to fetch.
    #[arg(long, default_value_t = 100, help_heading = "Traversal")]
    max_pages: usize,

    /// Maximum link depth from the seed; zero fetches only the seed.
    #[arg(long, default_value_t = 2, help_heading = "Traversal")]
    max_depth: usize,

    /// Maximum number of page requests in flight.
    #[arg(long, default_value_t = 8, help_heading = "Traversal")]
    concurrency: usize,

    /// Maximum links considered from each fetched page.
    #[arg(long, default_value_t = 1_000, help_heading = "Traversal")]
    max_links_per_page: usize,

    /// Frontier ordering policy.
    #[arg(long, value_enum, default_value_t = StrategyArg::Bfs, help_heading = "Traversal")]
    strategy: StrategyArg,

    /// Permit links to any domain instead of staying on the seed domain.
    #[arg(long, help_heading = "Scope")]
    allow_cross_domain: bool,

    /// Permit subdomains of the seed domain.
    #[arg(long, help_heading = "Scope")]
    allow_subdomains: bool,

    /// Follow only paths beginning with this prefix; repeatable.
    #[arg(long, value_name = "PATH", action = ArgAction::Append, help_heading = "Scope")]
    include_path_prefix: Vec<String>,

    /// Reject paths beginning with this prefix; repeatable.
    #[arg(long, value_name = "PATH", action = ArgAction::Append, help_heading = "Scope")]
    exclude_path_prefix: Vec<String>,

    /// Follow links marked rel=nofollow.
    #[arg(long, help_heading = "Politeness")]
    follow_nofollow: bool,

    /// Do not fetch or enforce robots.txt.
    #[arg(long, help_heading = "Politeness")]
    ignore_robots: bool,

    /// Minimum delay reserved between requests to one host and port.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "250ms",
        value_parser = parse_duration,
        help_heading = "Politeness"
    )]
    delay: Duration,

    /// Timeout for each HTTP request.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "30s",
        value_parser = parse_duration,
        help_heading = "Network"
    )]
    timeout: Duration,

    /// Maximum response body retained for one page.
    #[arg(
        long,
        value_name = "BYTES",
        default_value = "8MiB",
        value_parser = parse_byte_size,
        help_heading = "Network"
    )]
    max_download_bytes: usize,

    /// Maximum redirects followed for one page.
    #[arg(long, default_value_t = 5, help_heading = "Network")]
    max_redirects: u8,

    /// Maximum retries after a retryable request failure.
    #[arg(long, default_value_t = 2, help_heading = "Network")]
    max_retries: u8,

    /// Reject redirects that change scheme, host, or port.
    #[arg(long, help_heading = "Network")]
    deny_cross_origin_redirects: bool,

    /// Permit private, loopback, and link-local network targets.
    #[arg(long, help_heading = "Network")]
    allow_private_networks: bool,

    /// Value sent in the HTTP User-Agent header and used for robots rules.
    #[arg(long, default_value_t = default_user_agent(), help_heading = "Network")]
    user_agent: String,

    /// Validate and print the effective crawl plan without network access.
    #[arg(long, help_heading = "Automation")]
    dry_run: bool,

    /// Emit single-line JSON instead of pretty-printed JSON.
    #[arg(long, help_heading = "Automation")]
    compact: bool,

    /// Increase stderr diagnostics; repeat for debug-level logging.
    #[arg(short, long, action = ArgAction::Count, help_heading = "Automation")]
    verbose: u8,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StrategyArg {
    Bfs,
    Dfs,
}

impl Cli {
    fn crawl_config(&self) -> CrawlConfig {
        CrawlConfig {
            max_pages: self.max_pages,
            max_depth: self.max_depth,
            concurrency: self.concurrency,
            max_links_per_page: self.max_links_per_page,
            strategy: match self.strategy {
                StrategyArg::Bfs => CrawlStrategy::BreadthFirst,
                StrategyArg::Dfs => CrawlStrategy::DepthFirst,
            },
            stay_on_domain: !self.allow_cross_domain,
            allow_subdomains: self.allow_subdomains,
            follow_nofollow: self.follow_nofollow,
            respect_robots: !self.ignore_robots,
            include_path_prefixes: self.include_path_prefix.clone(),
            exclude_path_prefixes: self.exclude_path_prefix.clone(),
            default_delay: self.delay,
            request_timeout: self.timeout,
            max_download_bytes: self.max_download_bytes,
            max_redirects: self.max_redirects,
            max_retries: self.max_retries,
            allow_cross_origin_redirects: !self.deny_cross_origin_redirects,
            allow_private_networks: self.allow_private_networks,
            user_agent: self.user_agent.clone(),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let config = cli.crawl_config();

    if let Err(error) = config
        .validate()
        .and_then(|()| Crawler::validate_seed(&cli.url))
    {
        return report_crawl_error(&error);
    }
    if cli.dry_run {
        let plan = crawl_plan(&cli.url, &config);
        return emit_json(&plan, cli.compact);
    }

    let crawler = match Crawler::new(config) {
        Ok(crawler) => crawler,
        Err(error) => return report_crawl_error(&error),
    };
    match crawler.crawl(&cli.url).await {
        Ok(report) => emit_json(&report, cli.compact),
        Err(error) => report_crawl_error(&error),
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
        "schema_version": 1,
        "dry_run": true,
        "seed_url": url,
        "config": {
            "max_pages": config.max_pages,
            "max_depth": config.max_depth,
            "concurrency": config.concurrency,
            "max_links_per_page": config.max_links_per_page,
            "strategy": config.strategy,
            "stay_on_domain": config.stay_on_domain,
            "allow_subdomains": config.allow_subdomains,
            "follow_nofollow": config.follow_nofollow,
            "respect_robots": config.respect_robots,
            "include_path_prefixes": config.include_path_prefixes,
            "exclude_path_prefixes": config.exclude_path_prefixes,
            "default_delay_ms": duration_millis(config.default_delay),
            "request_timeout_ms": duration_millis(config.request_timeout),
            "max_download_bytes": config.max_download_bytes,
            "max_redirects": config.max_redirects,
            "max_retries": config.max_retries,
            "allow_cross_origin_redirects": config.allow_cross_origin_redirects,
            "allow_private_networks": config.allow_private_networks,
            "user_agent": config.user_agent,
        }
    })
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn emit_json(value: &impl Serialize, compact: bool) -> ExitCode {
    let result = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    };
    let json = match result {
        Ok(json) => json,
        Err(error) => {
            eprintln!("failed to serialize JSON output: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = io::stdout().lock();
    match writeln!(stdout, "{json}") {
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
        CrawlError::NetworkDenied(_) | CrawlError::Network(_) => ExitCode::from(4),
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

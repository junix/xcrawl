use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use url::Url;
use xcrawl::{CrawlConfig, CrawlStrategy, Crawler};

#[derive(Debug, Parser)]
#[command(version, about = "Bounded native web crawler")]
struct Cli {
    url: Url,
    #[arg(long, default_value_t = 100)]
    max_pages: usize,
    #[arg(long, default_value_t = 2)]
    max_depth: usize,
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    #[arg(long, value_enum, default_value_t = StrategyArg::Bfs)]
    strategy: StrategyArg,
    #[arg(long)]
    allow_subdomains: bool,
    #[arg(long)]
    allow_private_networks: bool,
    #[arg(long)]
    ignore_robots: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StrategyArg {
    Bfs,
    Dfs,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let config = CrawlConfig {
        max_pages: cli.max_pages,
        max_depth: cli.max_depth,
        concurrency: cli.concurrency,
        strategy: match cli.strategy {
            StrategyArg::Bfs => CrawlStrategy::BreadthFirst,
            StrategyArg::Dfs => CrawlStrategy::DepthFirst,
        },
        allow_subdomains: cli.allow_subdomains,
        allow_private_networks: cli.allow_private_networks,
        respect_robots: !cli.ignore_robots,
        ..CrawlConfig::default()
    };
    let result = match Crawler::new(config) {
        Ok(crawler) => crawler.crawl(&cli.url).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(report) => match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("failed to serialize crawl report: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

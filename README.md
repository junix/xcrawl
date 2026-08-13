# xcrawl

`xcrawl` is a policy-enforced, bounded Rust crawler for static HTTP(S) pages.
Traversal and acquisition live here; page decoding, link discovery, metadata,
and readability extraction are supplied by the default `ReadabilitiesAnalyzer`.

## Safety model

The policy boundary is one actual HTTP attempt. Before every page, redirect,
retry, or robots request, the runtime:

1. validates the URL, port, crawl scope, and URL-length budget;
2. reserves the global request/origin budget;
3. obtains the exact origin's rate and in-flight permit;
4. resolves through a shared timeout-bounded resolver and rejects denied CIDRs;
5. sends exactly one HTTP request and charges streamed bytes to the crawl;
6. records status, `Retry-After`, timing, and response bytes.

Redirect targets repeat the page-scope and robots checks before being requested.
Retries repeat the origin-scheduler acquisition. The default redirect policy is
`within_crawl_scope`, HTTPS downgrades are denied, the default scope is the
seed's exact origin, only ports 80 and 443 are enabled, and IANA non-global
address classes are denied.

Robots handling follows RFC 9309's access outcomes:

- a usable 2xx response applies its parsed rules;
- 4xx means unavailable and permits crawling;
- network errors and 5xx mean unreachable and disallow crawling.

Matching combines repeated product-token groups, normalizes percent-encoded
octets, supports `*` and `$`, and ignores blank/comment-only lines without
terminating a group. Remote crawl delays are parsed fallibly and capped at 60
seconds. The configured origin delay always remains a floor.

## Bounded work

Independent limits cover logical pages, HTTP attempts, response bytes, total
download bytes, unique origins, frontier entries, URL length, crawl duration,
attempt duration, robots size, reported links, and output bytes. Page analysis
runs on Tokio's blocking pool and receives the response body by move rather than
clone. The scheduler refills each free slot as soon as a page completes.

## CLI

JSON Lines is the default so page, failure, request, robots, and final summary
records can be consumed without collecting the crawl in memory:

```sh
xcrawl https://example.com \
  --max-pages 100 \
  --max-depth 2 \
  --concurrency 8 \
  --max-http-requests 1000 \
  --max-total-download-bytes 128MiB \
  --max-crawl-duration 10m
```

Use `--format json` for a bounded collected report. Query values are redacted
from reports by default; `--include-query-values` explicitly disables that
protection. `--dry-run` validates the complete grouped policy without touching
the network.

Important policy flags include:

| Policy | Flags |
|---|---|
| Traversal | `--max-pages`, `--max-depth`, `--concurrency`, `--max-origin-in-flight` |
| Links | `--max-links-to-analyze`, `--max-links-to-enqueue`, `--max-links-to-report` |
| Scope | `--domain-scope`, `--allow-cross-domain`, `--redirect-policy`, `--allow-https-downgrade` |
| Robots | `--ignore-robots`, `--max-robots-delay`, `--max-robots-bytes`, `--max-robots-redirects` |
| Retry | `--max-retries`, `--retry-base-delay`, `--retry-max-delay`, `--ignore-retry-after` |
| Network | `--dns-timeout`, `--timeout`, `--allow-private-networks`, `--allow-nonstandard-ports` |
| Global limits | `--max-http-requests`, `--max-total-download-bytes`, `--max-unique-origins`, `--max-frontier-entries`, `--max-url-length`, `--max-crawl-duration`, `--max-report-bytes` |
| Outcome | `--allow-partial`, `--fail-on-any-error` |

Exit codes are stable:

- `0`: complete, explicitly allowed partial, or broken-pipe cancellation;
- `1`: partial, deadline, output-budget, or fatal failure;
- `2`: command-line usage error;
- `3`: invalid URL or crawl policy;
- `4`: seed/network failure.

Private test services normally need both explicit opt-ins:

```sh
xcrawl http://127.0.0.1:8000 \
  --allow-private-networks \
  --allow-nonstandard-ports
```

## Library

The 0.2 API groups policy by responsibility and exposes crawler-owned analysis
DTOs. The default adapter is replaceable through `PageAnalyzer`.

```rust,no_run
use url::Url;
use xcrawl::{CrawlConfig, Crawler};

# async fn example() -> xcrawl::Result<()> {
let mut config = CrawlConfig::default();
config.limits.max_pages = 50;
config.traversal.max_depth = 2;

let report = Crawler::new(config)?
    .crawl(&Url::parse("https://example.com").unwrap())
    .await?;

println!("{:?}: {} pages", report.outcome, report.stats.pages_crawled);
# Ok(())
# }
```

`crawl_with_sink` streams `CrawlRecord` values without retaining pages or
events. `crawl_collect_with_frontier` injects an alternate `Frontier`; its
`enqueue_if_new` contract makes deduplication reservation and enqueue atomic.

Browser rendering, WAF bypass, distributed persistence, document downloads,
REST/MCP bindings, and LLM extraction remain outside this runtime.

## Development

```sh
just check-all
```

CI checks formatting, Clippy, tests, and builds on the declared Rust 1.85 MSRV.

## License

MIT. Adapted portions are documented in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

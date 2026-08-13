# xcrawl

`xcrawl` is a bounded native Rust web crawler. It owns traversal and HTTP
acquisition while [`readabilities-rs`](../readabilities/readabilities-rs)
owns page decoding, full-document link discovery, metadata, readability
selection, sanitization, and rendering.

Each URL is fetched exactly once. The immutable response is passed across the
explicit `PageSnapshot -> PageAnalysis` contract, so the crawler never calls
`readabilities-rs::read_url` and never discovers links from already-pruned
article HTML.

## Current runtime

- In-memory BFS or DFS frontier with normalized URL deduplication
- Bounded depth, page count, concurrency, response bytes, retries, redirects,
  and links per page
- Same-domain, subdomain, include-prefix, and exclude-prefix scope policies
- Per-domain delay, `Crawl-delay`, `Request-rate`, and adaptive 429 backoff
- robots.txt allow/disallow with longest-match, `*`, and `$` semantics
- Page and link `nofollow` handling
- DNS validation and address pinning with private-network SSRF denial
- IPv4-mapped IPv6 and NAT64 SSRF defense
- JSON CLI output containing pages, article outcomes, links, failures, events,
  and crawl statistics

Browser rendering, WAF bypass, distributed persistence, document downloads,
REST/MCP bindings, and LLM extraction are intentionally outside the initial
runtime. See [known divergences](alignment/known-divergences.md).

## CLI

```sh
xcrawl https://example.com \
  --max-pages 100 \
  --max-depth 2 \
  --concurrency 8 \
  --delay 500ms \
  --timeout 20s \
  --max-download-bytes 4MiB
```

The single command exposes the complete `CrawlConfig` policy:

| Group | Flags |
|---|---|
| Traversal | `--max-pages`, `--max-depth`, `--concurrency`, `--max-links-per-page`, `--strategy bfs\|dfs` |
| Scope | `--allow-cross-domain`, `--allow-subdomains`, repeatable `--include-path-prefix` and `--exclude-path-prefix` |
| Politeness | `--follow-nofollow`, `--ignore-robots`, `--delay 250ms` |
| Network | `--timeout 30s`, `--max-download-bytes 8MiB`, `--max-redirects`, `--max-retries`, `--deny-cross-origin-redirects`, `--allow-private-networks`, `--user-agent` |
| Automation | `--dry-run`, `--compact`, `-v`/`-vv` |

Durations accept `ms`, `s`, and `m`. Byte limits accept raw bytes or
`KB`/`KiB`, `MB`/`MiB`, and `GB`/`GiB`. `--dry-run` performs URL and policy
validation and prints the effective configuration without touching the
network:

```sh
xcrawl https://example.com \
  --include-path-prefix /docs \
  --exclude-path-prefix /docs/private \
  --dry-run --compact
```

stdout contains only the JSON crawl report (or dry-run plan); diagnostics go
to stderr. Exit codes are `0` success, `1` crawl/output failure, `2` command
usage, `3` invalid crawl policy, and `4` denied or unreachable network.

Private and loopback networks are denied by default. Local integration tests
may opt in explicitly:

```sh
xcrawl http://127.0.0.1:8000 --allow-private-networks
```

## Library

```rust,no_run
use url::Url;
use xcrawl::{CrawlConfig, Crawler};

# async fn example() -> xcrawl::Result<()> {
let crawler = Crawler::new(CrawlConfig {
    max_pages: 50,
    max_depth: 2,
    ..CrawlConfig::default()
})?;
let report = crawler
    .crawl(&Url::parse("https://example.com").unwrap())
    .await?;

for page in report.pages {
    if let Some(article) = page.article {
        println!("{}: {} words", page.final_url, article.word_count);
    }
}
# Ok(())
# }
```

Use `Crawler::with_reader(config, reader)` when page extraction needs custom
`readabilities-rs` options or site profiles. Crawl policy remains in
`CrawlConfig`; page-understanding policy remains in `Reader`.

## Development

```sh
just check-all
just install
```

## License

MIT. Portions adapted from Crawlberg are documented in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

# Known divergences from Crawlberg

| ID | Domain | Crawlberg | xcrawl | Reason | Since |
|---|---|---|---|---|---|
| D1 | page-understanding | Owns HTML metadata, links, content conversion, and pruning | Passes one immutable response snapshot to `readabilities-rs` | Keep traversal and page understanding as separate bounded contexts | 2026-08-12 |
| D2 | browser | Optional browser rendering and WAF bypass | Static HTTP acquisition only | Browser acquisition needs a separate explicit snapshot producer | 2026-08-12 |
| D3 | bindings | Fourteen language bindings plus REST and MCP | Native Rust library and CLI | Freeze the Rust crawl contract before multiplying integration surfaces | 2026-08-12 |
| D4 | documents | Downloads and processes PDF/Office/image assets | Follows HTTP(S) HTML pages only | Document processing is not part of the initial crawl runtime | 2026-08-12 |
| D5 | extraction | Optional LLM extraction and managed bypass tiers | Deterministic local analysis only | No hidden provider, model, billing, or remote-service dependency | 2026-08-12 |
| D6 | storage | Pluggable cache/store/event sinks | Injectable atomic frontier plus streaming event sink; default storage remains in-memory | Persisted/distributed leasing remains a separate implementation | 2026-08-12 |

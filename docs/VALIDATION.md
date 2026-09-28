# Acquisition review validation — 2026-09-27

Verified on macOS arm64 with Rust 1.98.0. `just build`, `just test`,
`just install`, `cargo fmt --all -- --check` and
`cargo clippy --all-targets -- -D warnings` passed. All 98 Rust tests passed:
43 library, 1 binary, 17 CLI, 3 configuration and 34 policy cases. Separately,
`cargo +1.88.0 check --locked --all-targets` passed at the declared minimum Rust
version. Logs are in [`../reports/validation/`](../reports/validation/).

Regression evidence covers full `Retry-After` and robots delays, cooldown
extension during a wait, later discovery at a shorter depth, partial output
budgets, unsupported content types, durable claim recovery, static media
discovery and URL redaction. The real CLI stdout test fills an unread pipe and
confirms the process exits within three seconds for a one-second crawl deadline.
The default in-process library adapter has the bounded admission contract
described in [acquisition.md](acquisition.md); hard cancellation uses processes.

Installed binaries at `/Users/junix/sync/macos-arm64-bin/xcrawl` and
`/Users/junix/sync/macos-arm64-bin/streamforge` passed a local cross-project test:

1. HTML discovery persists private signed URLs and response snapshots while
   public candidate metadata redacts secrets.
2. Reopening a completed durable frontier performs no duplicate fetch.
3. The private candidate downloads matching bytes, verifies media, retains
   provenance and replays without a second download.
4. A discovered HLS manifest runs through an isolated network-restricted worker
   with hard quotas and returns a verified artifact.

The runner and machine report are in the independent streamforge repository:
[`tests/xcrawl_integration.py`](../../streamforge/tests/xcrawl_integration.py)
and [`reports/xcrawl-integration.json`](../../streamforge/reports/xcrawl-integration.json).
They use explicitly allowed local fixtures; they do not establish public-site
extractor coverage. The shared address-policy corpus is tested in both projects.

`pm lint check -p xcrawl` has zero errors and one existing `DEP001` warning:
the pinned readabilities git revision is not in the local dependency checkout's
history. The lockfile's actual pinned source built and passed tests; this repair
does not silently change that upstream revision. `pm doctor --deep -p xcrawl`
reports only the intentional uncommitted changes. Installation is current.

Browser capture, authenticated session refresh, sitemap/feed enumeration,
collection workflows and resumable live segment ledgers remain coverage features.
No browser, public website, deployed system service or Linux crawler runtime was
validated in this run. Linux arm64 media worker containers were executed by the
streamforge isolation suite.

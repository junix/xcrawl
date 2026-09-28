# Recovery, deadlines and acquisition outputs

`xcrawl` remains a bounded static-page crawler. It now emits typed media
candidates from HTML video/audio/source elements, tracks, embeds, enclosure and
OpenGraph metadata, media links, and audio/video/manifest response headers.
Binary media is never admitted into page-body analysis. Pages record whether
analysis ran or their content type was skipped.

Use `--private-output DIR` to persist raw analyzed response bodies and private
execution candidates. Public pages include a relative `snapshot` reference.
`DIR/outbox/*.json` is the version 1 streamforge candidate contract, with stable
resource identity, source provenance, unmodified execution URL, kind and public
credential scope. The outbox is written before a page's frontier completion.
Duplicate discoveries preserve the original record so crash replay keeps an
identical idempotency request. Only private 0700 directories and 0600 candidate
files are accepted. Known public URL fields, article metadata/provenance URLs,
links, candidates and dry-run seeds redact query values and remove userinfo and
fragments. Raw bodies and article content are sensitive content; arbitrary text
is not promised to have been sanitized. Never submit public redacted JSON as a
download request.

```sh
xcrawl https://example.com --frontier-dir ./private/frontier \
  --private-output ./private/acquisition --max-pages 100
```

`--frontier-dir` selects a private, single-owner durable queue. An exclusive lock
prevents two crawls owning it at once. Claims, attempt counts, retry deadlines,
completion and discovered children are saved through synchronized atomic
replacement. Opening after a crash recovers claims from the previous owner;
graceful termination releases unfinished claims. A bound seed/scope/network/
depth identity prevents accidentally reusing another crawl's queue. A completed
seed is a valid resume and causes no refetch. Increasing coverage requires a new
frontier directory. This is local persistence, not a multi-worker database.

Minimum discovered depth replaces first-discovery depth. A later shorter path
requeues/reanalyzes a resource so descendants within the depth limit are not
lost to concurrent arrival order. Completion of a deeper claim cannot acknowledge
a pending shallower visit.

Normal page, request, byte, frontier and output budget exhaustion returns partial
results with `page_limit`, `request_limit`, `byte_limit`, `frontier_limit` or
`output_limit`. Exhausting the bounded eligible frontier reports
`frontier_exhausted`. Statistics distinguish fetched, analyzed, skipped, failed,
frontier-rejected and unfinished work. `--max-report-bytes` bounds retained
collected data; `--max-stream-bytes` separately bounds streamed records. Terminal
diagnostics have a small separate allowance. A blocked consumer can prevent
delivery of its summary, but cannot keep the CLI running indefinitely.

The CLI runs analysis and stdout emission in child processes. Parent-side async
I/O is deadline-aware; cancelled children are killed, and worker parent-liveness
checks also handle abrupt parent exit. Output acknowledgement follows actual
stdout flush, so pipe backpressure is visible to the deadline. The parser admits
at most 100,000 markup tokens, in addition to byte limits. Analysis workers have
a 30 CPU-second limit and, on Linux, a 1 GiB address-space limit. macOS does not
claim that Linux address-space guarantee. Input/output IPC has explicit limits.

Library `CrawlSink::emit` is now async. Implementations must yield to allow
cancellation. Trusted custom synchronous analyzers use a global four-thread
admission limit; cancellation releases the waiting future but holds each permit
until that computation actually finishes. Applications needing process-bound
analysis use `Crawler::with_analysis_worker(path_to_xcrawl)`. A custom synchronous
analyzer cannot be forcibly stopped in the host process.

Origin scheduling always honors the full server `Retry-After` and robots delay.
If a delay exceeds the remaining crawl duration, the crawl ends or defers rather
than retrying early. Waiters recheck a cooldown after waking. Unrepresentably
large delays defer the origin for the whole run. The legacy max-robots-delay
option is retained for CLI compatibility and no longer discards long delays.
Robots 429 is temporary unavailability, not an allow rule. The shared
`contracts/network-policy-v1.json` corpus checks the Rust and Go address policy,
including IPv6 documentation ranges and IPv4-embedded forms.

Browser capture, session refresh, sitemap/feed expansion, collections and live
recording are not implemented here. See streamforge's acquisition contract for
the isolated media boundary and its explicit backend capabilities.

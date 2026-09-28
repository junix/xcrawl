use std::collections::BTreeMap;

use readabilities_rs::{PageSnapshot, Reader};
use url::Url;

static ANALYSIS_SLOTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::OnceLock::new();

use crate::model::{
    AnalysisError, AnalysisWarning, AnalyzedArticle, AnalyzedLink, ArticleMetadata,
    ArticleProvenance, ArticleSignals, PageAnalysis, PageRobots,
};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PageInput {
    pub final_url: Url,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub response_headers: BTreeMap<String, String>,
    pub max_links: usize,
}

pub(crate) async fn analyze_bounded(
    analyzer: std::sync::Arc<dyn PageAnalyzer>,
    page: PageInput,
    worker: Option<&std::path::Path>,
) -> crate::Result<PageAnalysis> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // readabilities currently has no internal node/work limit. Bound parser
    // admission before invoking it, in addition to the response byte limit.
    if page
        .body
        .iter()
        .filter(|byte| **byte == b'<')
        .take(100_001)
        .count()
        > 100_000
    {
        return Err(crate::CrawlError::Analysis(
            "document exceeds 100000 markup tokens".to_string(),
        ));
    }
    if let Some(worker) = worker {
        let mut child = tokio::process::Command::new(worker)
            .arg("__xcrawl_analyze")
            .env("XCRAWL_PARENT_PID", std::process::id().to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| crate::CrawlError::Analysis(e.to_string()))?;
        let input =
            serde_json::to_vec(&page).map_err(|e| crate::CrawlError::Analysis(e.to_string()))?;
        child
            .stdin
            .take()
            .expect("piped input")
            .write_all(&input)
            .await
            .map_err(|e| crate::CrawlError::Analysis(e.to_string()))?;
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("piped output")
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|e| crate::CrawlError::Analysis(e.to_string()))?;
        if output.len() > 64 * 1024 * 1024 {
            return Err(crate::CrawlError::Analysis(
                "analysis output limit exceeded".to_string(),
            ));
        }
        if !child
            .wait()
            .await
            .map_err(|e| crate::CrawlError::Analysis(e.to_string()))?
            .success()
        {
            return Err(crate::CrawlError::Analysis(
                "analysis worker failed".to_string(),
            ));
        }
        return serde_json::from_slice(&output)
            .map_err(|e| crate::CrawlError::Analysis(e.to_string()));
    }
    // Trusted library analyzers cannot be forcibly cancelled. Keep a global
    // permit until the actual computation exits, across cancelled crawl runs.
    // Detached threads do not hold up Tokio runtime shutdown.
    let permit = ANALYSIS_SLOTS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
        .acquire_owned()
        .await
        .expect("analysis semaphore stays open");
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("xcrawl-analysis".to_string())
        .spawn(move || {
            let _permit = permit;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| analyzer.analyze(page)))
                    .map_err(|_| {
                        crate::CrawlError::Analysis("analysis worker panicked".to_string())
                    });
            let _ = sender.send(result);
        })
        .map_err(|e| crate::CrawlError::Analysis(e.to_string()))?;
    receiver
        .await
        .map_err(|e| crate::CrawlError::Analysis(e.to_string()))?
}

pub trait PageAnalyzer: Send + Sync {
    fn analyze(&self, page: PageInput) -> PageAnalysis;
}

#[derive(Debug, Clone)]
pub struct ReadabilitiesAnalyzer {
    reader: Reader,
}

impl ReadabilitiesAnalyzer {
    pub fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl Default for ReadabilitiesAnalyzer {
    fn default() -> Self {
        Self::new(Reader::new())
    }
}

impl PageAnalyzer for ReadabilitiesAnalyzer {
    fn analyze(&self, page: PageInput) -> PageAnalysis {
        let resources = crate::resource::discover(&page.final_url, &page.body, page.max_links);
        let mut snapshot = PageSnapshot::origin(page.final_url, page.content_type, page.body);
        snapshot.response_headers = page.response_headers;
        let mut analysis = self.reader.analyze_snapshot(snapshot);
        let links_discovered = analysis.links.len();
        analysis.links.truncate(page.max_links);
        let (article, article_error) = match analysis.article {
            Ok(article) => {
                let metadata = article.metadata;
                let signals = article.signals;
                let provenance = article.provenance;
                let article = AnalyzedArticle {
                    content: article.content,
                    metadata: ArticleMetadata {
                        title: metadata.title,
                        author: metadata.author,
                        description: metadata.description,
                        published: metadata.published,
                        modified: metadata.modified,
                        site: metadata.site,
                        language: metadata.language,
                        image: metadata.image,
                        canonical_url: metadata.canonical_url,
                        keywords: metadata.keywords,
                    },
                    word_count: article.word_count,
                    quality: format!("{:?}", article.quality).to_ascii_lowercase(),
                    signals: ArticleSignals {
                        words: signals.words,
                        text_chars: signals.text_chars,
                        paragraphs: signals.paragraphs,
                        headings: signals.headings,
                        links: signals.links,
                        code_blocks: signals.code_blocks,
                        tables: signals.tables,
                        score: signals.score,
                    },
                    provenance: ArticleProvenance {
                        engine: provenance.engine,
                        site_extractor: provenance.site_extractor,
                        site_config: provenance.site_config,
                        source_url: provenance.source_url,
                        degraded: provenance.degraded,
                    },
                    warnings: article
                        .warnings
                        .into_iter()
                        .map(|warning| AnalysisWarning {
                            code: warning.code,
                            message: warning.message,
                        })
                        .collect(),
                };
                (Some(article), None)
            }
            Err(error) => (
                None,
                Some(AnalysisError {
                    kind: format!("{:?}", error.kind).to_ascii_lowercase(),
                    stage: format!("{:?}", error.stage).to_ascii_lowercase(),
                    message: error.message,
                    retry: format!("{:?}", error.retry).to_ascii_lowercase(),
                }),
            ),
        };
        PageAnalysis {
            resources,
            article,
            article_error,
            links: analysis
                .links
                .into_iter()
                .map(|link| AnalyzedLink {
                    url: link.url,
                    text: link.text,
                    rel: link.rel,
                    nofollow: link.nofollow,
                })
                .collect(),
            canonical_url: analysis.canonical_url,
            robots: PageRobots {
                noindex: analysis.robots.noindex,
                nofollow: analysis.robots.nofollow,
            },
            detected_encoding: analysis.detected_encoding,
            decode_errors: analysis.decode_errors,
            links_discovered,
        }
    }
}

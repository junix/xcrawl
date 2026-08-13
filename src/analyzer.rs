use std::collections::BTreeMap;

use readabilities_rs::{PageSnapshot, Reader};
use url::Url;

use crate::model::{
    AnalysisError, AnalysisWarning, AnalyzedArticle, AnalyzedLink, ArticleMetadata,
    ArticleProvenance, ArticleSignals, PageAnalysis, PageRobots,
};

#[derive(Debug)]
pub struct PageInput {
    pub final_url: Url,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub response_headers: BTreeMap<String, String>,
    pub max_links: usize,
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

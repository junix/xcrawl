//! Enumeration documents: sitemaps (including nested indexes and gzip-wrapped
//! files) and RSS/Atom feeds. Designated seeds are expanded into ordinary
//! frontier entries, so every discovered URL still passes the same scope,
//! robots, budget, and depth machinery as a page link.

use std::io::Read;

use quick_xml::Reader;
use std::borrow::Cow;

use quick_xml::events::Event;
use url::Url;

use crate::model::{EntryKind, RequestKind};
use crate::{CrawlError, Result};

/// A child URL extracted from an enumeration document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnumerationChild {
    pub url: Url,
    /// Nested sitemap indexes expand into further sitemap entries; everything
    /// else becomes an ordinary page.
    pub kind: EntryKind,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnumerationDocument {
    pub children: Vec<EnumerationChild>,
    /// Total child locations seen before truncation, mirroring link counts.
    pub total_discovered: usize,
    pub truncated: bool,
}

/// Request kind used for every designated enumeration document.
pub(crate) fn request_kind(kind: EntryKind) -> RequestKind {
    match kind {
        EntryKind::Page => RequestKind::Page,
        EntryKind::Sitemap | EntryKind::Feed => RequestKind::Enumeration,
    }
}

/// Inflate a gzip-wrapped enumeration body (a stored `.gz` document, not a
/// `Content-Encoding` transfer, which the HTTP client already decodes).
/// Decompression is capped so a small compressed body cannot bypass the
/// response byte limit.
pub(crate) fn inflate_gzip(body: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let mut decoder = flate2::read::MultiGzDecoder::new(body).take(max_bytes as u64 + 1);
    let mut inflated = Vec::new();
    decoder
        .read_to_end(&mut inflated)
        .map_err(|error| CrawlError::Analysis(format!("invalid gzip document: {error}")))?;
    if inflated.len() > max_bytes {
        return Err(CrawlError::ResourceBudget {
            resource: "response_bytes",
            limit: max_bytes,
        });
    }
    Ok(inflated)
}

fn analysis_error(message: impl Into<String>) -> CrawlError {
    CrawlError::Analysis(message.into())
}

/// Parse one enumeration document. `max_children` bounds the retained child
/// list, matching the link-analysis bound for pages.
pub(crate) fn parse_document(
    kind: EntryKind,
    base: &Url,
    body: &[u8],
    max_children: usize,
) -> Result<EnumerationDocument> {
    let body = if body.starts_with(&[0x1f, 0x8b]) {
        // A stored gzip document may hold several uncompressed entries' worth
        // of URLs; the cap scales with the retained child bound while staying
        // a fraction of the ordinary page-body limit.
        inflate_gzip(body, max_children.saturating_mul(8_192).max(1024 * 1024))?
    } else {
        body.to_vec()
    };
    match kind {
        EntryKind::Sitemap => parse_sitemap(base, &body, max_children),
        EntryKind::Feed => parse_feed(base, &body, max_children),
        EntryKind::Page => Err(analysis_error("pages are not enumeration documents")),
    }
}

fn parse_sitemap(base: &Url, body: &[u8], max_children: usize) -> Result<EnumerationDocument> {
    let text = String::from_utf8_lossy(body);
    let mut reader = Reader::from_str(&text);
    let mut stack: Vec<String> = Vec::new();
    let mut index_root = false;
    let mut root_seen = false;
    let mut document = EnumerationDocument {
        children: Vec::new(),
        total_discovered: 0,
        truncated: false,
    };
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.local_name().into_inner().to_string();
                if stack.is_empty() {
                    root_seen = true;
                    match name.as_str() {
                        "urlset" => index_root = false,
                        "sitemapindex" => index_root = true,
                        other => {
                            return Err(analysis_error(format!(
                                "not a sitemap document; root element is {other}"
                            )));
                        }
                    }
                }
                stack.push(name);
                text.clear();
            }
            Ok(Event::End(_)) => {
                let closed = stack.pop();
                if closed.as_deref() == Some("loc") {
                    let child_kind = if index_root {
                        EntryKind::Sitemap
                    } else {
                        EntryKind::Page
                    };
                    let inside_index_entry = !index_root && path_ends_with(&stack, &["url"]);
                    let inside_sitemap_entry = index_root && path_ends_with(&stack, &["sitemap"]);
                    if inside_index_entry || inside_sitemap_entry {
                        push_child(base, &text, child_kind, "", max_children, &mut document);
                    }
                }
                text.clear();
            }
            Ok(Event::Text(value)) => {
                let decoded = value.xml10_content();
                append_location(&mut text, &stack, decoded.trim_matches(is_whitespace));
            }
            Ok(Event::CData(value)) => {
                append_location(
                    &mut text,
                    &stack,
                    value.as_ref().trim_matches(is_whitespace),
                );
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(character) = decode_general_ref(&reference) {
                    append_location(&mut text, &stack, &character.to_string());
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(analysis_error(format!("invalid sitemap XML: {error}"))),
        }
    }
    if !root_seen {
        return Err(analysis_error("empty sitemap document"));
    }
    Ok(document)
}

fn parse_feed(base: &Url, body: &[u8], max_children: usize) -> Result<EnumerationDocument> {
    let text = String::from_utf8_lossy(body);
    let mut reader = Reader::from_str(&text);
    let mut stack: Vec<String> = Vec::new();
    let mut atom_root = false;
    let mut root_seen = false;
    let mut document = EnumerationDocument {
        children: Vec::new(),
        total_discovered: 0,
        truncated: false,
    };
    let mut text = String::new();
    let mut entry_title = String::new();
    let mut atom_alternate: Option<Url> = None;
    let mut atom_fallback: Option<Url> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.local_name().into_inner().to_string();
                if stack.is_empty() {
                    root_seen = true;
                    match name.as_str() {
                        "rss" | "RDF" => atom_root = false,
                        "feed" => atom_root = true,
                        other => {
                            return Err(analysis_error(format!(
                                "not an RSS or Atom feed; root element is {other}"
                            )));
                        }
                    }
                }
                if atom_root
                    && name == "link"
                    && path_ends_with(&stack, &["entry"])
                    && let Some((alternate, url)) = atom_link_attribute(base, &event)
                {
                    // Prefer rel="alternate"; keep the first link of any
                    // rel as a fallback for entries without one.
                    if alternate {
                        atom_alternate = url.or(atom_alternate);
                    } else if atom_fallback.is_none() {
                        atom_fallback = url;
                    }
                }
                stack.push(name);
                text.clear();
            }
            Ok(Event::End(_)) => {
                let closed = stack.pop();
                let Some(closed) = closed.as_deref() else {
                    text.clear();
                    continue;
                };
                if atom_root {
                    match closed {
                        "entry" => {
                            let link = atom_alternate.take().or(atom_fallback.take());
                            if let Some(link) = link {
                                push_child(
                                    base,
                                    link.as_str(),
                                    EntryKind::Page,
                                    &entry_title,
                                    max_children,
                                    &mut document,
                                );
                            }
                            entry_title.clear();
                        }
                        "title" if path_ends_with(&stack, &["entry"]) => {
                            entry_title = text.trim().to_string();
                        }
                        _ => {}
                    }
                } else {
                    match closed {
                        "link" if path_ends_with(&stack, &["item"]) => {
                            push_child(
                                base,
                                &text,
                                EntryKind::Page,
                                &entry_title,
                                max_children,
                                &mut document,
                            );
                        }
                        "title" if path_ends_with(&stack, &["item"]) => {
                            entry_title = text.trim().to_string();
                        }
                        _ => {}
                    }
                }
                text.clear();
            }
            Ok(Event::Text(value)) => {
                let decoded = value.xml10_content();
                text.push_str(&decoded);
            }
            Ok(Event::CData(value)) => {
                text.push_str(value.as_ref());
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(character) = decode_general_ref(&reference) {
                    text.push(character);
                }
            }
            Ok(Event::Empty(event)) => {
                // Self-closing links are common in Atom entries.
                let name = event.local_name().into_inner();
                if atom_root
                    && name == "link"
                    && path_ends_with(&stack, &["entry"])
                    && let Some((alternate, url)) = atom_link_attribute(base, &event)
                {
                    if alternate {
                        atom_alternate = url.or(atom_alternate);
                    } else if atom_fallback.is_none() {
                        atom_fallback = url;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(analysis_error(format!("invalid feed XML: {error}"))),
        }
    }
    if !root_seen {
        return Err(analysis_error("empty feed document"));
    }
    Ok(document)
}

fn atom_link_attribute(
    base: &Url,
    event: &quick_xml::events::BytesStart<'_>,
) -> Option<(bool, Option<Url>)> {
    let mut rel: Option<String> = None;
    let mut href: Option<String> = None;
    for attribute in event.attributes().filter_map(std::result::Result::ok) {
        match attribute.key.as_ref() {
            "rel" => {
                rel = attribute
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .ok()
                    .map(Cow::into_owned);
            }
            "href" => {
                href = attribute
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .ok()
                    .map(Cow::into_owned);
            }
            _ => {}
        }
    }
    let href = href?;
    let alternate = rel.as_deref().is_none_or(|rel| rel == "alternate");
    Some((alternate, base.join(&href).ok()))
}

/// Decode a predefined XML entity or character reference. Unknown entities
/// expand to nothing rather than aborting a well-formed document.
fn decode_general_ref(reference: &str) -> Option<char> {
    match reference {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => {}
    }
    let digits = reference.strip_prefix('#')?;
    let code = if let Some(hex) = digits
        .strip_prefix('x')
        .or_else(|| digits.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        digits.parse::<u32>().ok()?
    };
    char::from_u32(code)
}

fn append_location(text: &mut String, stack: &[String], fragment: &str) {
    if path_ends_with(stack, &["url", "loc"]) || path_ends_with(stack, &["sitemap", "loc"]) {
        text.push_str(fragment);
    }
}

fn path_ends_with(stack: &[String], suffix: &[&str]) -> bool {
    stack.len() >= suffix.len()
        && stack[stack.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(actual, expected)| actual == expected)
}

fn is_whitespace(character: char) -> bool {
    character.is_whitespace()
}

fn push_child(
    base: &Url,
    raw: &str,
    kind: EntryKind,
    title: &str,
    max_children: usize,
    document: &mut EnumerationDocument,
) {
    let raw = raw.trim_matches(is_whitespace);
    if raw.is_empty() {
        return;
    }
    let Ok(mut url) = base.join(raw) else {
        return;
    };
    url.set_fragment(None);
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return;
    }
    document.total_discovered = document.total_discovered.saturating_add(1);
    if document.children.len() >= max_children {
        document.truncated = true;
        return;
    }
    document.children.push(EnumerationChild {
        url,
        kind,
        title: title.trim().chars().take(500).collect(),
    });
}

#[cfg(test)]
#[path = "enumeration_tests.rs"]
mod tests;

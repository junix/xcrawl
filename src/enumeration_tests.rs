use std::fmt::Write as _;

use super::*;
use crate::EntryKind;
use url::Url;

fn base() -> Url {
    Url::parse("https://example.test/").unwrap()
}

fn child_urls(document: &EnumerationDocument) -> Vec<String> {
    document
        .children
        .iter()
        .map(|child| child.url.to_string())
        .collect()
}

#[test]
fn urlset_yields_page_children() {
    let body = br#"<?xml version="1.0"?>
        <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
          <url><loc>https://example.test/a</loc><lastmod>2026-01-01</lastmod></url>
          <url><loc>/relative/b</loc></url>
        </urlset>"#;
    let document = parse_document(EntryKind::Sitemap, &base(), body, 100).unwrap();
    assert_eq!(
        child_urls(&document),
        vec!["https://example.test/a", "https://example.test/relative/b"]
    );
    assert!(
        document
            .children
            .iter()
            .all(|child| child.kind == EntryKind::Page)
    );
    assert!(!document.truncated);
}

#[test]
fn sitemap_index_yields_sitemap_children() {
    let body = br#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
          <sitemap><loc>https://example.test/sitemap-1.xml</loc></sitemap>
          <sitemap><loc>https://example.test/sitemap-2.xml</loc></sitemap>
        </sitemapindex>"#;
    let document = parse_document(EntryKind::Sitemap, &base(), body, 100).unwrap();
    assert_eq!(document.children.len(), 2);
    assert!(
        document
            .children
            .iter()
            .all(|child| child.kind == EntryKind::Sitemap)
    );
}

#[test]
fn gzip_wrapped_sitemap_inflates() {
    use std::io::Write;
    let raw = br"<urlset><url><loc>https://example.test/gz</loc></url></urlset>";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(raw).unwrap();
    let compressed = encoder.finish().unwrap();
    let document = parse_document(EntryKind::Sitemap, &base(), &compressed, 100).unwrap();
    assert_eq!(
        child_urls(&document),
        vec!["https://example.test/gz".to_string()]
    );
}

#[test]
fn sitemap_rejects_other_roots_and_empty_bodies() {
    let html = br"<html><body>not a sitemap</body></html>";
    let error = parse_document(EntryKind::Sitemap, &base(), html, 10).unwrap_err();
    assert!(error.to_string().contains("not a sitemap document"));
    assert!(
        parse_document(EntryKind::Sitemap, &base(), b"", 10)
            .unwrap_err()
            .to_string()
            .contains("empty sitemap document")
    );
}

#[test]
fn sitemap_entities_and_non_http_locations_are_dropped_or_decoded() {
    let body = b"<?xml version='1.0'?><urlset><url><loc>https://example.test/a?x=1&amp;y=2</loc></url><url><loc>ftp://example.test/x</loc></url><url><loc>javascript:alert(1)</loc></url></urlset>";
    let document = parse_document(EntryKind::Sitemap, &base(), body, 100).unwrap();
    assert_eq!(
        child_urls(&document),
        vec!["https://example.test/a?x=1&y=2".to_string()]
    );
    // Rejected locations still count as discovered, mirroring link filtering.
    assert_eq!(document.total_discovered, 1);
}

#[test]
fn sitemap_child_bound_truncates_honestly() {
    let mut body = String::from("<urlset>");
    for index in 0..10 {
        let _ = write!(body, "<url><loc>https://example.test/p{index}</loc></url>");
    }
    body.push_str("</urlset>");
    let document = parse_document(EntryKind::Sitemap, &base(), body.as_bytes(), 3).unwrap();
    assert_eq!(document.children.len(), 3);
    assert_eq!(document.total_discovered, 10);
    assert!(document.truncated);
}

#[test]
fn rss_items_yield_links_and_titles() {
    let body = br#"<?xml version="1.0"?>
        <rss version="2.0"><channel><title>Feed</title>
          <item><title>First</title><link>https://example.test/one</link></item>
          <item><title>Second</title><link>/two</link><guid>ignored</guid></item>
        </channel></rss>"#;
    let document = parse_document(EntryKind::Feed, &base(), body, 100).unwrap();
    assert_eq!(
        child_urls(&document),
        vec!["https://example.test/one", "https://example.test/two"]
    );
    assert_eq!(document.children[0].title, "First");
    assert_eq!(document.children[1].title, "Second");
}

#[test]
fn rss_cdata_links_are_read() {
    let body = br#"<rss version="2.0"><channel>
          <item><title>Escaped</title><link><![CDATA[https://example.test/cdata]]></link></item>
        </channel></rss>"#;
    let document = parse_document(EntryKind::Feed, &base(), body, 100).unwrap();
    assert_eq!(
        child_urls(&document),
        vec!["https://example.test/cdata".to_string()]
    );
}

#[test]
fn atom_entries_prefer_alternate_links() {
    let body = br#"<?xml version="1.0"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <entry><title>Enclosure entry</title>
            <link rel="enclosure" href="https://cdn.example.test/media.mp3"/>
            <link rel="alternate" href="https://example.test/entry"/>
          </entry>
          <entry><title>Fallback entry</title>
            <link rel="related" href="https://example.test/related"/>
          </entry>
        </feed>"#;
    let document = parse_document(EntryKind::Feed, &base(), body, 100).unwrap();
    assert_eq!(
        child_urls(&document),
        vec![
            "https://example.test/entry".to_string(),
            "https://example.test/related".to_string()
        ]
    );
    assert_eq!(document.children[0].title, "Enclosure entry");
}

#[test]
fn feed_rejects_other_roots() {
    let error = parse_document(EntryKind::Feed, &base(), b"<html></html>", 10).unwrap_err();
    assert!(error.to_string().contains("not an RSS or Atom feed"));
}

#[test]
fn gzip_inflation_is_bounded() {
    use std::io::Write;
    let raw = vec![b'a'; 4 * 1024 * 1024];
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&raw).unwrap();
    let compressed = encoder.finish().unwrap();
    let error = inflate_gzip(&compressed, 1024).unwrap_err();
    assert!(error.to_string().contains("response_bytes"));
}

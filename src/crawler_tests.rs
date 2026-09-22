use super::*;

#[test]
fn segment_path_prefix_has_a_boundary() {
    assert!(path_prefix_matches(
        "/docs/page",
        "/docs",
        PathMatchMode::SegmentPrefix
    ));
    assert!(!path_prefix_matches(
        "/docs-old",
        "/docs",
        PathMatchMode::SegmentPrefix
    ));
}

#[test]
fn path_prefix_modes_split_on_segment_boundaries_only() {
    for (path, prefix, mode, expected) in [
        ("/", "/", PathMatchMode::SegmentPrefix, true),
        ("/anything/deep", "/", PathMatchMode::SegmentPrefix, true),
        ("/docs", "/docs", PathMatchMode::SegmentPrefix, true),
        ("/docs/page", "/docs", PathMatchMode::SegmentPrefix, true),
        ("/docs/page", "/docs/", PathMatchMode::SegmentPrefix, true),
        ("/docs", "/docs/", PathMatchMode::SegmentPrefix, false),
        ("/docs-old", "/docs", PathMatchMode::SegmentPrefix, false),
        // Raw prefix is a literal string match, with no segment care.
        ("/docs-old", "/docs", PathMatchMode::RawPrefix, true),
        ("/docs/page", "/docs", PathMatchMode::RawPrefix, true),
        ("/other", "/docs", PathMatchMode::RawPrefix, false),
    ] {
        assert_eq!(
            path_prefix_matches(path, prefix, mode),
            expected,
            "{path} vs {prefix} in {mode:?}"
        );
    }
}

#[test]
fn normalization_is_idempotent_and_fragment_insensitive() {
    let url = Url::parse("https://EXAMPLE.test/a#one").unwrap();
    let once = normalize_url(&url);
    let twice = normalize_url(&Url::parse(&once).unwrap());
    assert_eq!(once, twice);
    assert_eq!(
        once,
        normalize_url(&Url::parse("https://example.test/a#two").unwrap())
    );
}

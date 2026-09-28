use super::*;

#[test]
fn discovers_typed_media_and_preserves_signed_execution_urls() {
    let source = Url::parse("https://example.com/page").unwrap();
    let candidates = discover(&source, br#"<base href="/assets/"><video src="movie.mp4?sig=secret"></video><source src="live.m3u8"><track src="en.vtt"><iframe src="https://video.example/embed/1"></iframe><a href="/ordinary">link</a>"#, 10);
    assert_eq!(candidates.len(), 4);
    assert_eq!(
        candidates[0].url.as_str(),
        "https://example.com/assets/movie.mp4?sig=secret"
    );
    assert_eq!(candidates[0].kind, "direct_media");
    assert_eq!(candidates[1].kind, "hls");
    assert_eq!(candidates[2].kind, "subtitle");
    assert_eq!(candidates[3].kind, "media_page");
    assert_eq!(
        candidates[0].resource_id,
        identity(&candidates[0].url, "direct_media")
    );
    assert!(
        discover(
            &source,
            b"<video src='file:///secret'><video src='blob:abc'>",
            10
        )
        .is_empty()
    );
}

#[test]
fn binary_headers_route_without_parsing_the_body() {
    let url = Url::parse("https://example.com/download?sig=secret").unwrap();
    assert_eq!(
        from_response(&url, &url, Some("video/mp4"))[0].kind,
        "direct_media"
    );
    assert_eq!(
        from_response(&url, &url, Some("application/dash+xml"))[0].kind,
        "dash"
    );
    assert!(from_response(&url, &url, None).is_empty());
    assert_eq!(
        discover(&url, b"<video src='/without-extension'>", 10)[0].kind,
        "direct_media"
    );
}

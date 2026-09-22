use super::*;

#[tokio::test]
async fn enqueue_and_seen_reservation_are_atomic() {
    let frontier = InMemoryFrontier::new(CrawlStrategy::BreadthFirst, 2);
    let entry = FrontierEntry {
        url: Url::parse("https://example.test/a#fragment").unwrap(),
        depth: 3,
    };
    let first = frontier.enqueue_if_new(vec![entry.clone()]).await.unwrap();
    assert_eq!(first.enqueued.len(), 1);
    assert_eq!(first.enqueued[0].depth, 3);
    assert_eq!(first.duplicates, 0);
    assert_eq!(first.rejected_capacity, 0);
    let duplicate = frontier.enqueue_if_new(vec![entry]).await.unwrap();
    assert!(duplicate.enqueued.is_empty());
    assert_eq!(duplicate.duplicates, 1);
    assert_eq!(duplicate.rejected_capacity, 0);
    let popped = frontier.pop().await.unwrap().unwrap();
    assert_eq!(popped.url.path(), "/a");
    assert_eq!(popped.depth, 3);
    assert!(frontier.is_empty().await.unwrap());
}

#[tokio::test]
async fn capacity_bounds_seen_and_queue_state() {
    let frontier = InMemoryFrontier::new(CrawlStrategy::BreadthFirst, 1);
    let entries = ["a", "b"]
        .into_iter()
        .map(|path| FrontierEntry {
            url: Url::parse(&format!("https://example.test/{path}")).unwrap(),
            depth: 0,
        })
        .collect();
    let result = frontier.enqueue_if_new(entries).await.unwrap();
    assert_eq!(result.enqueued.len(), 1);
    assert_eq!(result.enqueued[0].url.path(), "/a");
    assert_eq!(result.rejected_capacity, 1);
    assert!(!frontier.is_empty().await.unwrap());
    // A capacity-rejected URL is not recorded as seen, so re-offering it
    // while the frontier is still full is another capacity rejection,
    // not a duplicate.
    let retry = frontier
        .enqueue_if_new(vec![FrontierEntry {
            url: Url::parse("https://example.test/b").unwrap(),
            depth: 0,
        }])
        .await
        .unwrap();
    assert!(retry.enqueued.is_empty());
    assert_eq!(retry.rejected_capacity, 1);
    assert_eq!(retry.duplicates, 0);
}

#[tokio::test]
async fn normalization_variations_dedupe_to_one_entry() {
    let frontier = InMemoryFrontier::new(CrawlStrategy::BreadthFirst, 4);
    let variants = [
        "https://example.test/docs#a",
        "https://example.test/docs#b",
        "https://EXAMPLE.test/docs",
    ];
    let entries = variants
        .into_iter()
        .map(|raw| FrontierEntry {
            url: Url::parse(raw).unwrap(),
            depth: 0,
        })
        .collect();
    let result = frontier.enqueue_if_new(entries).await.unwrap();
    // Fragments are stripped and host case is canonicalized before the
    // dedup key is formed, so all three spellings are one URL.
    assert_eq!(result.enqueued.len(), 1);
    assert_eq!(result.duplicates, 2);
    assert_eq!(result.rejected_capacity, 0);
}

#[tokio::test]
async fn pop_order_follows_the_configured_strategy() {
    for (strategy, expected) in [
        (CrawlStrategy::BreadthFirst, ["/a", "/b", "/c"]),
        (CrawlStrategy::DepthFirst, ["/c", "/b", "/a"]),
    ] {
        let frontier = InMemoryFrontier::new(strategy, 3);
        let entries = ["a", "b", "c"]
            .into_iter()
            .map(|path| FrontierEntry {
                url: Url::parse(&format!("https://example.test/{path}")).unwrap(),
                depth: 0,
            })
            .collect();
        assert_eq!(
            frontier.enqueue_if_new(entries).await.unwrap().enqueued.len(),
            3
        );
        for path in expected {
            assert_eq!(
                frontier.pop().await.unwrap().unwrap().url.path(),
                path,
                "{strategy:?}"
            );
        }
        assert!(frontier.pop().await.unwrap().is_none());
    }
}

use super::*;
use crate::EntryKind;
use url::Url;

fn directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "xcrawl-frontier-{label}-{}-{}",
        std::process::id(),
        now_ms()
    ))
}

fn entry(path: &str, depth: usize) -> FrontierEntry {
    FrontierEntry {
        url: Url::parse(&format!("https://example.test/{path}")).unwrap(),
        depth,
        kind: EntryKind::Page,
    }
}

#[tokio::test]
async fn a_crashed_claim_is_recovered_and_children_commit_with_completion() {
    let directory = directory("claims");
    let seed = entry("seed", 0);
    {
        let frontier = DurableFrontier::open(&directory, 10).unwrap();
        assert!(DurableFrontier::open(&directory, 10).is_err());
        frontier.enqueue_if_new(vec![seed.clone()]).await.unwrap();
        assert_eq!(frontier.pop().await.unwrap(), Some(seed.clone()));
    }
    {
        let frontier = DurableFrontier::open(&directory, 10).unwrap();
        assert_eq!(frontier.pop().await.unwrap(), Some(seed.clone()));
        frontier
            .complete(&seed, vec![entry("child", 1)])
            .await
            .unwrap();
    }
    {
        let frontier = DurableFrontier::open(&directory, 10).unwrap();
        assert_eq!(
            frontier
                .enqueue_if_new(vec![seed])
                .await
                .unwrap()
                .duplicates,
            1
        );
        assert_eq!(frontier.pop().await.unwrap(), Some(entry("child", 1)));
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn validators_survive_reopening_and_stay_bounded_to_admitted_entries() {
    let directory = directory("validators");
    let sitemap = FrontierEntry {
        url: Url::parse("https://example.test/sitemap.xml").unwrap(),
        depth: 0,
        kind: EntryKind::Sitemap,
    };
    {
        let frontier = DurableFrontier::open(&directory, 10).unwrap();
        frontier
            .enqueue_if_new(vec![sitemap.clone()])
            .await
            .unwrap();
        // Validators for an admitted entry persist across reopen.
        frontier
            .store_validators(
                &sitemap.url,
                &FrontierValidators {
                    etag: Some("\"v1\"".to_string()),
                    last_modified: Some("Mon, 05 Oct 2026 00:00:00 GMT".to_string()),
                },
            )
            .await
            .unwrap();
    }
    {
        let frontier = DurableFrontier::open(&directory, 10).unwrap();
        let validators = frontier.validators(&sitemap.url).await.unwrap();
        assert_eq!(
            validators,
            Some(FrontierValidators {
                etag: Some("\"v1\"".to_string()),
                last_modified: Some("Mon, 05 Oct 2026 00:00:00 GMT".to_string()),
            })
        );
        // Unknown and unadmitted URLs never retain validator state.
        let stranger = Url::parse("https://example.test/other.xml").unwrap();
        frontier
            .store_validators(
                &stranger,
                &FrontierValidators {
                    etag: Some("\"x\"".to_string()),
                    last_modified: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(frontier.validators(&stranger).await.unwrap(), None);
        assert_eq!(
            frontier
                .validators(&Url::parse("https://example.test/missing.xml").unwrap())
                .await
                .unwrap(),
            None
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

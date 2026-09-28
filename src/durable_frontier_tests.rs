use super::*;
use url::Url;

fn directory() -> PathBuf {
    std::env::temp_dir().join(format!(
        "xcrawl-frontier-{}-{}",
        std::process::id(),
        now_ms()
    ))
}

fn entry(path: &str, depth: usize) -> FrontierEntry {
    FrontierEntry {
        url: Url::parse(&format!("https://example.test/{path}")).unwrap(),
        depth,
    }
}

#[tokio::test]
async fn a_crashed_claim_is_recovered_and_children_commit_with_completion() {
    let directory = directory();
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

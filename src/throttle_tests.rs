use super::*;

// Paused tokio time only advances inside `sleep`, so the measured
// intervals are exactly the reserved delays, not merely lower bounds.
#[tokio::test(start_paused = true)]
async fn configured_delay_is_always_a_floor() {
    for robots in [
        Duration::ZERO,
        Duration::from_millis(100),
        Duration::from_secs(1),
    ] {
        let scheduler = OriginScheduler::new(Duration::from_millis(250), 1);
        let first = Instant::now();
        drop(scheduler.acquire("example:443").await);
        scheduler.set_robots_delay("example:443", robots);
        drop(scheduler.acquire("example:443").await);
        let elapsed = Instant::now().duration_since(first);
        assert_eq!(
            elapsed,
            Duration::from_millis(250).max(robots),
            "robots delay {robots:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn zero_floor_still_backs_off_after_429() {
    let scheduler = OriginScheduler::new(Duration::ZERO, 1);
    drop(scheduler.acquire("example:443").await);
    scheduler.record_response("example:443", 429, None);
    let start = Instant::now();
    drop(scheduler.acquire("example:443").await);
    assert_eq!(
        Instant::now().duration_since(start),
        Duration::from_millis(200)
    );
}

#[tokio::test(start_paused = true)]
async fn consecutive_429s_double_the_backoff() {
    let scheduler = OriginScheduler::new(Duration::ZERO, 1);
    drop(scheduler.acquire("example:443").await);
    for expected in [200, 400, 800, 1600].map(Duration::from_millis) {
        scheduler.record_response("example:443", 429, None);
        let start = Instant::now();
        drop(scheduler.acquire("example:443").await);
        assert_eq!(Instant::now().duration_since(start), expected);
    }
}

#[tokio::test(start_paused = true)]
async fn backoff_growth_is_capped_at_sixty_seconds() {
    let scheduler = OriginScheduler::new(Duration::ZERO, 1);
    drop(scheduler.acquire("example:443").await);
    for _ in 0..10 {
        scheduler.record_response("example:443", 429, None);
    }
    let start = Instant::now();
    drop(scheduler.acquire("example:443").await);
    assert_eq!(
        Instant::now().duration_since(start),
        Duration::from_secs(60)
    );
}

#[tokio::test(start_paused = true)]
async fn five_successes_halve_the_adaptive_backoff() {
    let scheduler = OriginScheduler::new(Duration::ZERO, 1);
    drop(scheduler.acquire("example:443").await);
    scheduler.record_response("example:443", 429, None);
    let start = Instant::now();
    drop(scheduler.acquire("example:443").await);
    assert_eq!(
        Instant::now().duration_since(start),
        Duration::from_millis(200)
    );
    for _ in 0..5 {
        scheduler.record_response("example:443", 200, None);
    }
    // Burns the reservation made before the halving took effect.
    drop(scheduler.acquire("example:443").await);
    let start = Instant::now();
    drop(scheduler.acquire("example:443").await);
    assert_eq!(
        Instant::now().duration_since(start),
        Duration::from_millis(100)
    );
}

#[tokio::test(start_paused = true)]
async fn retry_after_blocks_the_next_attempt() {
    let scheduler = OriginScheduler::new(Duration::ZERO, 1);
    drop(scheduler.acquire("example:443").await);
    scheduler.record_response("example:443", 503, Some(Duration::from_secs(3)));
    let start = Instant::now();
    drop(scheduler.acquire("example:443").await);
    assert_eq!(
        Instant::now().duration_since(start),
        Duration::from_secs(3)
    );
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_capped_at_sixty_seconds() {
    let scheduler = OriginScheduler::new(Duration::ZERO, 1);
    drop(scheduler.acquire("example:443").await);
    scheduler.record_response(
        "example:443",
        503,
        Some(Duration::from_secs(5 * 60)),
    );
    let start = Instant::now();
    drop(scheduler.acquire("example:443").await);
    assert_eq!(
        Instant::now().duration_since(start),
        Duration::from_secs(60)
    );
}

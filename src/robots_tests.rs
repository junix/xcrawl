use super::*;
use proptest::prelude::*;

#[test]
fn blank_lines_comments_and_repeated_groups_are_combined() {
    let rules = RobotsRules::parse(
        "User-agent: xcrawl\n\n# inside group\nDisallow: /private\n\n\
             User-agent: xcrawl\nDisallow: /other\n",
    );
    assert!(!rules.allowed("xcrawl/0.1", "/private/a"));
    assert!(!rules.allowed("xcrawl/0.1", "/other/a"));
}

#[test]
fn percent_encoding_wildcards_and_equal_allow_are_conformant() {
    let rules = RobotsRules::parse(
        "User-agent: *\nDisallow: /foo*$\nDisallow: /caf%C3%A9\n\
             Disallow: /x%62\nAllow: /xb\n",
    );
    assert!(!rules.allowed("xcrawl", "/foobar"));
    assert!(!rules.allowed("xcrawl", "/caf%C3%A9/menu"));
    assert!(rules.allowed("xcrawl", "/xb"));
}

#[test]
fn request_rate_uses_numerator_and_extreme_delays_are_ignored() {
    let rules = RobotsRules::parse(
        "User-agent: a\nRequest-rate: 100/10\n\
             User-agent: b\nCrawl-delay: 1e300\n",
    );
    assert_eq!(rules.crawl_delay("a"), Some(Duration::from_millis(100)));
    assert_eq!(rules.crawl_delay("b"), None);
}

#[test]
fn longer_rule_wins_over_shorter_regardless_of_polarity() {
    let rules = RobotsRules::parse(
        "User-agent: *\nDisallow: /p\nAllow: /p/sub\n\
             Allow: /a\nDisallow: /a/private\n",
    );
    assert!(rules.allowed("xcrawl", "/p/sub"));
    assert!(!rules.allowed("xcrawl", "/p/other"));
    assert!(!rules.allowed("xcrawl", "/a/private"));
    assert!(rules.allowed("xcrawl", "/a/public"));
}

#[test]
fn dollar_anchor_matches_only_the_exact_path() {
    let rules = RobotsRules::parse("User-agent: *\nDisallow: /foo$\n");
    assert!(!rules.allowed("xcrawl", "/foo"));
    assert!(rules.allowed("xcrawl", "/foobar"));
    assert!(rules.allowed("xcrawl", "/foo/bar"));
}

#[test]
fn most_specific_agent_group_wins_over_the_star_group() {
    let rules = RobotsRules::parse(
        "User-agent: webcrawler\nDisallow: /specific\n\
             User-agent: *\nDisallow: /star\n",
    );
    assert!(!rules.allowed("webcrawler/1.0", "/specific"));
    assert!(rules.allowed("webcrawler/1.0", "/star"));
    // A group only applies when its agent token is contained in the
    // product token, case-insensitively.
    assert!(!rules.allowed("WEBCRAWLER/1.0", "/specific"));
    assert!(!rules.allowed("otherbot/1.0", "/star"));
    assert!(rules.allowed("otherbot/1.0", "/specific"));
}

#[test]
fn empty_directive_values_are_ignored() {
    // Empty Allow/Disallow values add no rule, so everything stays allowed.
    let rules = RobotsRules::parse("User-agent: *\nDisallow:\nAllow: \n");
    assert!(rules.allowed("xcrawl", "/anything"));
    // An empty User-agent value does not start a new group, so the rule
    // still binds to the preceding agent.
    let grouped = RobotsRules::parse("User-agent: x\nUser-agent: \nDisallow: /p\n");
    assert!(!grouped.allowed("x", "/p"));
}

#[test]
fn sitemap_directives_are_collected_across_groups() {
    let rules = RobotsRules::parse(
        "Sitemap: https://a.test/sitemap.xml\n\
             User-agent: *\nDisallow: /private\n\
             Sitemap: https://b.test/sitemap.xml\nSitemap:\n",
    );
    assert_eq!(
        rules.sitemaps,
        ["https://a.test/sitemap.xml", "https://b.test/sitemap.xml"]
    );
    assert!(!rules.allowed("xcrawl", "/private/a"));
}

#[test]
fn crawl_delay_wins_over_request_rate_and_bad_values_are_ignored() {
    let rules = RobotsRules::parse(
        "User-agent: both\nCrawl-delay: 2\nRequest-rate: 100/1\n\
             User-agent: negative\nCrawl-delay: -5\n\
             User-agent: zero-rate\nRequest-rate: 0/10\n\
             User-agent: malformed\nRequest-rate: fast\n",
    );
    assert_eq!(rules.crawl_delay("both"), Some(Duration::from_secs(2)));
    assert_eq!(rules.crawl_delay("negative"), None);
    assert_eq!(rules.crawl_delay("zero-rate"), None);
    assert_eq!(rules.crawl_delay("malformed"), None);
}

#[test]
fn reserved_escapes_are_uppercased_but_not_decoded() {
    // %2F encodes the reserved '/', so RFC 9309 keeps it escaped instead
    // of decoding it into a path separator.
    let rules = RobotsRules::parse("User-agent: *\nDisallow: /a%2fb\n");
    assert!(!rules.allowed("xcrawl", "/a%2Fb"));
    assert!(rules.allowed("xcrawl", "/a/b"));
}

proptest! {
    #[test]
    fn arbitrary_input_never_panics(input in any::<Vec<u8>>()) {
        let input = String::from_utf8_lossy(&input);
        let rules = RobotsRules::parse(&input);
        let _ = rules.allowed("xcrawl", "/path?q=1");
    }
}

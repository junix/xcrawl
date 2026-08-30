use std::sync::Arc;
use std::time::Duration;

use crate::RobotsDecision;

const DEFAULT_MAX_ROBOTS_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub enum RobotsState {
    Rules {
        rules: Arc<RobotsRules>,
        status: u16,
    },
    UnavailableAllow {
        status: u16,
    },
    UnreachableDisallow {
        status: Option<u16>,
        error: String,
    },
}

impl RobotsState {
    pub(crate) fn allowed(&self, user_agent: &str, path_and_query: &str) -> bool {
        match self {
            Self::Rules { rules, .. } => rules.allowed(user_agent, path_and_query),
            Self::UnavailableAllow { .. } => true,
            Self::UnreachableDisallow { .. } => false,
        }
    }

    pub(crate) fn crawl_delay(&self, user_agent: &str) -> Option<Duration> {
        match self {
            Self::Rules { rules, .. } => rules.crawl_delay(user_agent),
            Self::UnavailableAllow { .. } | Self::UnreachableDisallow { .. } => None,
        }
    }

    pub(crate) fn event_fields(&self) -> (RobotsDecision, Option<u16>, Option<String>) {
        match self {
            Self::Rules { status, .. } => (RobotsDecision::Rules, Some(*status), None),
            Self::UnavailableAllow { status } => {
                (RobotsDecision::UnavailableAllow, Some(*status), None)
            }
            Self::UnreachableDisallow { status, error } => (
                RobotsDecision::UnreachableDisallow,
                *status,
                Some(error.clone()),
            ),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RobotsRules {
    groups: Vec<Group>,
    pub sitemaps: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<PathRule>,
    crawl_delay: Option<Duration>,
}

#[derive(Debug, Clone)]
struct PathRule {
    allow: bool,
    pattern: String,
    specificity: usize,
}

impl RobotsRules {
    pub fn parse(input: &str) -> Self {
        Self::parse_with_max_delay(input, DEFAULT_MAX_ROBOTS_DELAY)
    }

    pub fn parse_with_max_delay(input: &str, max_delay: Duration) -> Self {
        let mut result = Self::default();
        let mut group = Group::default();
        let mut saw_group_field = false;

        for raw_line in input.lines() {
            let line = raw_line.split('#').next().unwrap_or_default().trim();
            // RFC 9309 permits empty and comment-only lines inside a group.
            if line.is_empty() {
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            match name.as_str() {
                "user-agent" => {
                    if value.is_empty() {
                        continue;
                    }
                    if saw_group_field && !group.agents.is_empty() {
                        result.groups.push(std::mem::take(&mut group));
                        saw_group_field = false;
                    }
                    group.agents.push(value.to_ascii_lowercase());
                }
                "allow" | "disallow" if !group.agents.is_empty() => {
                    saw_group_field = true;
                    if value.is_empty() {
                        continue;
                    }
                    let pattern = normalize_for_match(value);
                    let specificity = pattern
                        .strip_suffix('$')
                        .unwrap_or(&pattern)
                        .bytes()
                        .filter(|byte| *byte != b'*')
                        .count();
                    group.rules.push(PathRule {
                        allow: name == "allow",
                        pattern,
                        specificity,
                    });
                }
                "crawl-delay" if !group.agents.is_empty() => {
                    saw_group_field = true;
                    group.crawl_delay = parse_delay(value, max_delay);
                }
                "request-rate" if !group.agents.is_empty() => {
                    saw_group_field = true;
                    if group.crawl_delay.is_none() {
                        group.crawl_delay = parse_request_rate(value, max_delay);
                    }
                }
                "sitemap" if !value.is_empty() => result.sitemaps.push(value.to_string()),
                _ => {}
            }
        }
        if !group.agents.is_empty() {
            result.groups.push(group);
        }
        result
    }

    pub fn allowed(&self, user_agent: &str, path_and_query: &str) -> bool {
        let path = normalize_for_match(path_and_query);
        self.applicable_groups(user_agent)
            .flat_map(|group| &group.rules)
            .filter(|rule| robots_path_matches(&path, &rule.pattern))
            .max_by_key(|rule| (rule.specificity, rule.allow))
            .is_none_or(|rule| rule.allow)
    }

    pub fn crawl_delay(&self, user_agent: &str) -> Option<Duration> {
        self.applicable_groups(user_agent)
            .filter_map(|group| group.crawl_delay)
            .max()
    }

    fn applicable_groups<'a>(
        &'a self,
        user_agent: &'a str,
    ) -> impl Iterator<Item = &'a Group> + 'a {
        let user_agent = user_agent.to_ascii_lowercase();
        let best = self
            .groups
            .iter()
            .flat_map(|group| &group.agents)
            .filter(|agent| agent.as_str() != "*" && user_agent.contains(agent.as_str()))
            .map(String::len)
            .max();
        self.groups.iter().filter(move |group| match best {
            Some(specificity) => group
                .agents
                .iter()
                .any(|agent| agent.len() == specificity && user_agent.contains(agent)),
            None => group.agents.iter().any(|agent| agent == "*"),
        })
    }
}

fn parse_delay(value: &str, max_delay: Duration) -> Option<Duration> {
    value
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .filter(|delay| *delay <= max_delay)
}

fn parse_request_rate(value: &str, max_delay: Duration) -> Option<Duration> {
    let (requests, seconds) = value.split_once('/')?;
    let requests = requests.trim().parse::<f64>().ok()?;
    let seconds = seconds.trim().parse::<f64>().ok()?;
    if !requests.is_finite() || !seconds.is_finite() || requests <= 0.0 || seconds < 0.0 {
        return None;
    }
    parse_delay(&(seconds / requests).to_string(), max_delay)
}

fn robots_path_matches(path: &str, rule: &str) -> bool {
    let (pattern, exact_end) = rule
        .strip_suffix('$')
        .map_or((rule, false), |pattern| (pattern, true));
    let mut pattern = pattern.as_bytes().to_vec();
    if !exact_end {
        pattern.push(b'*');
    }
    wildcard_match(path.as_bytes(), &pattern)
}

fn wildcard_match(value: &[u8], pattern: &[u8]) -> bool {
    let (mut value_index, mut pattern_index) = (0, 0);
    let (mut star_index, mut star_value_index) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && pattern[pattern_index] != b'*'
            && pattern[pattern_index] == value[value_index]
        {
            value_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            star_value_index += 1;
            value_index = star_value_index;
            pattern_index = star + 1;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

/// Normalize URI octets according to RFC 9309 section 2.2.2. Percent-encoded
/// unreserved ASCII is decoded; other escapes are retained with uppercase hex;
/// non-ASCII UTF-8 bytes are percent encoded.
fn normalize_for_match(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                let decoded = high * 16 + low;
                if decoded.is_ascii() && is_unreserved(decoded) {
                    result.push(char::from(decoded));
                } else {
                    result.push('%');
                    result.push(hex_digit(decoded >> 4));
                    result.push(hex_digit(decoded & 0x0f));
                }
                index += 3;
                continue;
            }
        }
        let byte = bytes[index];
        if byte.is_ascii() {
            result.push(char::from(byte));
        } else {
            result.push('%');
            result.push(hex_digit(byte >> 4));
            result.push(hex_digit(byte & 0x0f));
        }
        index += 1;
    }
    result
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_digit(value: u8) -> char {
    char::from(if value < 10 {
        b'0' + value
    } else {
        b'A' + value - 10
    })
}

#[cfg(test)]
mod tests {
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
            [
                "https://a.test/sitemap.xml",
                "https://b.test/sitemap.xml"
            ]
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
}

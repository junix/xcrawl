use std::time::Duration;

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
    path: String,
}

impl RobotsRules {
    pub fn parse(input: &str) -> Self {
        let mut result = Self::default();
        let mut group = Group::default();
        let mut saw_group_rule = false;

        for line in input.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                if !group.agents.is_empty() {
                    result.groups.push(std::mem::take(&mut group));
                }
                saw_group_rule = false;
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            match name.as_str() {
                "user-agent" => {
                    if saw_group_rule && !group.agents.is_empty() {
                        result.groups.push(std::mem::take(&mut group));
                        saw_group_rule = false;
                    }
                    group.agents.push(value.to_ascii_lowercase());
                }
                "allow" | "disallow" if !group.agents.is_empty() => {
                    saw_group_rule = true;
                    if !value.is_empty() {
                        group.rules.push(PathRule {
                            allow: name == "allow",
                            path: value.to_string(),
                        });
                    }
                }
                "crawl-delay" if !group.agents.is_empty() => {
                    saw_group_rule = true;
                    group.crawl_delay = value
                        .parse::<f64>()
                        .ok()
                        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                        .map(Duration::from_secs_f64);
                }
                "request-rate" if !group.agents.is_empty() => {
                    saw_group_rule = true;
                    if group.crawl_delay.is_none() {
                        group.crawl_delay = value
                            .split_once('/')
                            .and_then(|(_, seconds)| seconds.trim().parse::<f64>().ok())
                            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                            .map(Duration::from_secs_f64);
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
        let Some(group) = self.select_group(user_agent) else {
            return true;
        };
        group
            .rules
            .iter()
            .filter(|rule| robots_path_matches(path_and_query, &rule.path))
            .max_by_key(|rule| (rule.path.len(), rule.allow))
            .is_none_or(|rule| rule.allow)
    }

    pub fn crawl_delay(&self, user_agent: &str) -> Option<Duration> {
        self.select_group(user_agent)
            .and_then(|group| group.crawl_delay)
            .or_else(|| {
                self.groups
                    .iter()
                    .find(|group| group.agents.iter().any(|agent| agent == "*"))
                    .and_then(|group| group.crawl_delay)
            })
    }

    fn select_group(&self, user_agent: &str) -> Option<&Group> {
        let user_agent = user_agent.to_ascii_lowercase();
        self.groups
            .iter()
            .filter_map(|group| {
                let specificity = group
                    .agents
                    .iter()
                    .filter_map(|agent| {
                        if agent == "*" {
                            Some(0)
                        } else if user_agent.contains(agent) {
                            Some(agent.len())
                        } else {
                            None
                        }
                    })
                    .max()?;
                Some((specificity, group))
            })
            .max_by_key(|(specificity, _)| *specificity)
            .map(|(_, group)| group)
    }
}

fn robots_path_matches(path: &str, rule: &str) -> bool {
    let (rule, exact_end) = rule
        .strip_suffix('$')
        .map_or((rule, false), |rule| (rule, true));
    if !rule.contains('*') {
        return if exact_end {
            path == rule
        } else {
            path.starts_with(rule)
        };
    }

    let mut remaining = path;
    for (index, segment) in rule.split('*').enumerate() {
        if segment.is_empty() {
            continue;
        }
        let Some(position) = remaining.find(segment) else {
            return false;
        };
        if index == 0 && position != 0 {
            return false;
        }
        remaining = &remaining[position + segment.len()..];
    }
    !exact_end || remaining.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_rule_and_specific_agent_win() {
        let rules = RobotsRules::parse(
            "User-agent: *\nDisallow: /private\nAllow: /private/public\n\n\
             User-agent: xcrawl\nDisallow: /xcrawl-only\nCrawl-delay: 0.5\n\n\
             Sitemap: https://example.test/sitemap.xml\n",
        );
        assert!(rules.allowed("xcrawl/0.1", "/private/secret"));
        assert!(!rules.allowed("xcrawl/0.1", "/xcrawl-only/page"));
        assert_eq!(
            rules.crawl_delay("xcrawl/0.1"),
            Some(Duration::from_millis(500))
        );
        assert_eq!(rules.sitemaps, ["https://example.test/sitemap.xml"]);

        assert!(!rules.allowed("other", "/private/secret"));
        assert!(rules.allowed("other", "/private/public/page"));
    }

    #[test]
    fn wildcard_end_anchor_and_request_rate_are_supported() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /*.pdf$\nRequest-rate: 1/2\n");
        assert!(!rules.allowed("xcrawl", "/files/report.pdf"));
        assert!(rules.allowed("xcrawl", "/files/report.pdf.html"));
        assert_eq!(rules.crawl_delay("xcrawl"), Some(Duration::from_secs(2)));
    }
}

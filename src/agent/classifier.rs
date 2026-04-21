use crate::config::schema::QueryClassificationConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationDecision {
    pub hint: String,
    pub priority: i32,
}

/// Classify a user message against the configured rules and return the
/// matching hint string, if any.
///
/// Returns `None` when classification is disabled, no rules are configured,
/// or no rule matches the message.
pub fn classify(config: &QueryClassificationConfig, message: &str) -> Option<String> {
    classify_with_decision(config, message).map(|decision| decision.hint)
}

/// Classify a user message and return the matched hint together with
/// match metadata for observability.
pub fn classify_with_decision(
    config: &QueryClassificationConfig,
    message: &str,
) -> Option<ClassificationDecision> {
    if !config.enabled || config.rules.is_empty() {
        return None;
    }

    let lower = message.to_lowercase();
    let len = message.len();

    let mut rules: Vec<_> = config.rules.iter().collect();
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));

    for rule in rules {
        // Length constraints
        if let Some(min) = rule.min_length {
            if len < min {
                continue;
            }
        }
        if let Some(max) = rule.max_length {
            if len > max {
                continue;
            }
        }

        // Check keywords (case-insensitive) and patterns (case-sensitive)
        let keyword_hit = rule
            .keywords
            .iter()
            .any(|kw: &String| lower.contains(&kw.to_lowercase()));
        let pattern_hit = rule
            .patterns
            .iter()
            .any(|pat: &String| message.contains(pat.as_str()));

        if keyword_hit || pattern_hit {
            return Some(ClassificationDecision {
                hint: rule.hint.clone(),
                priority: rule.priority,
            });
        }
    }

    None
}

/// Estimate message complexity on a scale of 0-100.
/// Used for dynamic model routing — simple tasks go to cheap models.
pub fn estimate_complexity(message: &str) -> u32 {
    let mut score: u32 = 0;
    let len = message.len();

    // Length-based: longer messages tend to be more complex
    if len > 2000 {
        score += 30;
    } else if len > 500 {
        score += 15;
    } else if len > 100 {
        score += 5;
    }

    let lower = message.to_lowercase();

    // Code-related keywords suggest higher complexity
    let complex_keywords = [
        "refactor",
        "architect",
        "design",
        "debug",
        "optimize",
        "implement",
        "migrate",
        "security",
        "performance",
        "concurrent",
    ];
    for kw in &complex_keywords {
        if lower.contains(kw) {
            score += 10;
        }
    }

    // Simple task keywords
    let simple_keywords = [
        "list",
        "show",
        "what is",
        "how to",
        "explain",
        "summarize",
        "read",
        "find",
        "search",
        "check",
    ];
    for kw in &simple_keywords {
        if lower.contains(kw) {
            score = score.saturating_sub(5);
        }
    }

    // Multiple questions or requirements increase complexity
    let question_marks =
        u32::try_from(message.chars().filter(|c| *c == '?').count()).unwrap_or(u32::MAX);
    score += question_marks.min(3) * 5;

    // Code blocks suggest complexity
    if message.contains("```") {
        score += 15;
    }

    score.min(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{ClassificationRule, QueryClassificationConfig};

    fn make_config(enabled: bool, rules: Vec<ClassificationRule>) -> QueryClassificationConfig {
        QueryClassificationConfig { enabled, rules }
    }

    #[test]
    fn disabled_returns_none() {
        let config = make_config(
            false,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hello".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "hello"), None);
    }

    #[test]
    fn empty_rules_returns_none() {
        let config = make_config(true, vec![]);
        assert_eq!(classify(&config, "hello"), None);
    }

    #[test]
    fn keyword_match_case_insensitive() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hello".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "HELLO world"), Some("fast".into()));
    }

    #[test]
    fn pattern_match_case_sensitive() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "code".into(),
                patterns: vec!["fn ".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "fn main()"), Some("code".into()));
        assert_eq!(classify(&config, "FN MAIN()"), None);
    }

    #[test]
    fn length_constraints() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hi".into()],
                max_length: Some(10),
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "hi"), Some("fast".into()));
        assert_eq!(
            classify(&config, "hi there, how are you doing today?"),
            None
        );

        let config2 = make_config(
            true,
            vec![ClassificationRule {
                hint: "reasoning".into(),
                keywords: vec!["explain".into()],
                min_length: Some(20),
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config2, "explain"), None);
        assert_eq!(
            classify(&config2, "explain how this works in detail"),
            Some("reasoning".into())
        );
    }

    #[test]
    fn priority_ordering() {
        let config = make_config(
            true,
            vec![
                ClassificationRule {
                    hint: "fast".into(),
                    keywords: vec!["code".into()],
                    priority: 1,
                    ..Default::default()
                },
                ClassificationRule {
                    hint: "code".into(),
                    keywords: vec!["code".into()],
                    priority: 10,
                    ..Default::default()
                },
            ],
        );
        assert_eq!(classify(&config, "write some code"), Some("code".into()));
    }

    #[test]
    fn no_match_returns_none() {
        let config = make_config(
            true,
            vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["hello".into()],
                ..Default::default()
            }],
        );
        assert_eq!(classify(&config, "something completely different"), None);
    }

    #[test]
    fn classify_with_decision_exposes_priority_of_matched_rule() {
        let config = make_config(
            true,
            vec![
                ClassificationRule {
                    hint: "fast".into(),
                    keywords: vec!["code".into()],
                    priority: 3,
                    ..Default::default()
                },
                ClassificationRule {
                    hint: "code".into(),
                    keywords: vec!["code".into()],
                    priority: 10,
                    ..Default::default()
                },
            ],
        );

        let decision = classify_with_decision(&config, "write code now")
            .expect("classification decision expected");
        assert_eq!(decision.hint, "code");
        assert_eq!(decision.priority, 10);
    }

    #[test]
    fn complexity_simple_question() {
        assert!(estimate_complexity("what is rust?") < 20);
    }

    #[test]
    fn complexity_complex_task() {
        // "refactor" (+10) + "concurrent" (+10) = 20
        // Adding more complex keywords to push over 40
        assert!(
            estimate_complexity(
                "refactor and optimize the authentication system to use async concurrent handlers, debug performance issues, and migrate to the new design"
            ) > 40
        );
    }

    #[test]
    fn complexity_caps_at_100() {
        // Length > 2000 (+30) plus 5 complex keywords (+50) plus code block (+15) = 95
        // Add question marks to push to 100
        let long_complex = format!(
            "refactor architect design debug optimize {}```code```???",
            "x ".repeat(1000)
        );
        assert_eq!(estimate_complexity(&long_complex), 100);
    }
}

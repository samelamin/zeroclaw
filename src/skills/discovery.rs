//! Semantic skill discovery using keyword matching.

use crate::skills::Skill;

/// Convert a skill into indexable fields.
/// Returns (title=name, content=description+prompts, tags).
pub fn skill_to_index_entry(skill: &Skill) -> (String, String, Vec<String>) {
    let title = skill.name.clone();

    let mut content_parts: Vec<String> = vec![skill.description.clone()];
    for prompt in &skill.prompts {
        content_parts.push(prompt.clone());
    }
    let content = content_parts.join(" ");

    let tags = skill.tags.clone();

    (title, content, tags)
}

/// Keyword relevance score (0.0–1.0) between query terms and skill text.
/// Counts how many query terms appear in lowercased skill_text.
/// Returns matches/total as fraction.
pub fn keyword_relevance(query_terms: &[&str], skill_text: &str) -> f64 {
    if query_terms.is_empty() {
        return 0.0;
    }

    let lower_text = skill_text.to_lowercase();
    let matches = query_terms
        .iter()
        .filter(|&&term| lower_text.contains(&term.to_lowercase()))
        .count();

    matches as f64 / query_terms.len() as f64
}

/// Find the best matching skills for a user query.
/// Splits query into terms, scores each skill by combining name + description + tags,
/// sorts by score descending, truncates to top_k, and filters out zero scores.
pub fn find_relevant_skills<'a>(
    query: &str,
    skills: &'a [Skill],
    top_k: usize,
) -> Vec<(&'a Skill, f64)> {
    let query_terms: Vec<&str> = query.split_whitespace().collect();

    if query_terms.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(&Skill, f64)> = skills
        .iter()
        .map(|skill| {
            let (title, content, tags) = skill_to_index_entry(skill);
            let searchable = format!("{} {} {}", title, content, tags.join(" "));
            let score = keyword_relevance(&query_terms, &searchable);
            (skill, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);

    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;

    fn make_skill(name: &str, description: &str, tags: Vec<&str>, prompts: Vec<&str>) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            version: "0.1.0".to_string(),
            author: None,
            tags: tags.into_iter().map(|t| t.to_string()).collect(),
            tools: vec![],
            prompts: prompts.into_iter().map(|p| p.to_string()).collect(),
            location: None,
        }
    }

    #[test]
    fn build_skill_index_entries() {
        let skill = make_skill(
            "update-menu",
            "Update restaurant menu items and prices",
            vec!["restaurant", "menu"],
            vec![],
        );
        let (title, content, tags) = skill_to_index_entry(&skill);

        assert_eq!(title, "update-menu");
        assert!(
            content.to_lowercase().contains("restaurant menu"),
            "content should contain 'restaurant menu', got: {content}"
        );
        assert!(tags.contains(&"restaurant".to_string()));
        assert!(tags.contains(&"menu".to_string()));
    }

    #[test]
    fn relevance_score_nonzero_for_matching_query() {
        let terms = vec!["menu", "update", "ramadan"];
        let text = "update restaurant menu items";
        let score = keyword_relevance(&terms, text);
        assert!(score > 0.0, "expected score > 0.0, got {score}");
    }

    #[test]
    fn relevance_score_zero_for_unrelated_query() {
        let terms = vec!["database", "migration", "postgres"];
        let text = "update restaurant menu items";
        let score = keyword_relevance(&terms, text);
        assert!(score < 0.01, "expected score < 0.01, got {score}");
    }

    #[test]
    fn find_relevant_skills_returns_sorted() {
        let skills = vec![
            make_skill(
                "unrelated",
                "This is about something else entirely",
                vec![],
                vec![],
            ),
            make_skill(
                "update-menu",
                "Update restaurant menu items and prices",
                vec!["restaurant", "menu"],
                vec![],
            ),
            make_skill(
                "view-menu",
                "View current menu items",
                vec!["menu"],
                vec![],
            ),
        ];

        let results = find_relevant_skills("update restaurant menu", &skills, 3);

        assert!(!results.is_empty(), "expected at least one result");
        // The first result should have the highest score
        let top_score = results[0].1;
        for (_, score) in &results {
            assert!(
                *score <= top_score,
                "results should be sorted descending by score"
            );
        }
        // The best match should be "update-menu" since it matches most terms
        assert_eq!(results[0].0.name, "update-menu");
    }

    #[test]
    fn find_relevant_skills_respects_top_k() {
        let skills = vec![
            make_skill("skill-a", "menu items for restaurant", vec!["menu"], vec![]),
            make_skill("skill-b", "menu update tool", vec!["menu"], vec![]),
            make_skill("skill-c", "restaurant menu prices", vec!["menu"], vec![]),
            make_skill("skill-d", "menu configuration", vec!["menu"], vec![]),
            make_skill("skill-e", "menu display system", vec!["menu"], vec![]),
        ];

        let results = find_relevant_skills("menu", &skills, 2);
        assert_eq!(results.len(), 2, "expected exactly 2 results");
    }
}

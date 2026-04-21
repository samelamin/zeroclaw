//! Skill export/import types for fleet sharing via Naseyma Studio.
//!
//! Exposes types and helpers for Studio to pull auto-generated skills
//! from individual ZeroClaw instances and distribute them across the fleet.

use serde::{Deserialize, Serialize};

/// Payload for exporting a skill to Studio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExportPayload {
    pub name: String,
    pub description: String,
    pub version: String,
    pub tags: Vec<String>,
    pub content: String,
    pub industry: Option<String>,
    pub source_instance: String,
}

/// Summary of a skill for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub tags: Vec<String>,
    pub description: String,
}

/// Build skill summaries from loaded skills.
pub fn build_skill_summaries(skills: &[crate::skills::Skill]) -> Vec<SkillSummary> {
    skills.iter().map(|s| SkillSummary {
        name: s.name.clone(),
        tags: s.tags.clone(),
        description: s.description.clone(),
    }).collect()
}

/// Build an export payload for a specific skill.
pub fn build_export_payload(
    skill: &crate::skills::Skill,
    content: &str,
    source_instance: &str,
) -> SkillExportPayload {
    SkillExportPayload {
        name: skill.name.clone(),
        description: skill.description.clone(),
        version: skill.version.clone(),
        tags: skill.tags.clone(),
        content: content.to_string(),
        industry: None,
        source_instance: source_instance.to_string(),
    }
}

/// Filter skills to only those that are auto-generated (tagged).
pub fn filter_exportable(summaries: &[SkillSummary]) -> Vec<&SkillSummary> {
    summaries.iter()
        .filter(|s| s.tags.contains(&"auto-generated".to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;

    fn make_skill(name: &str, tags: Vec<String>) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("Description for {}", name),
            version: "1.0.0".to_string(),
            author: None,
            tags,
            tools: vec![],
            prompts: vec![],
            location: None,
        }
    }

    #[test]
    fn serialize_skill_export_payload() {
        let payload = SkillExportPayload {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            version: "1.0.0".to_string(),
            tags: vec!["auto-generated".to_string()],
            content: "skill content here".to_string(),
            industry: None,
            source_instance: "instance-abc".to_string(),
        };

        let json = serde_json::to_string(&payload).expect("serialization failed");
        assert!(json.contains("\"name\":\"test-skill\""));
        assert!(json.contains("\"version\":\"1.0.0\""));
        assert!(json.contains("\"source_instance\":\"instance-abc\""));
        assert!(json.contains("\"content\":\"skill content here\""));
    }

    #[test]
    fn build_skill_summaries_maps_all_fields() {
        let skills = vec![
            make_skill("skill-one", vec!["tag-a".to_string()]),
            make_skill("skill-two", vec!["auto-generated".to_string()]),
        ];

        let summaries = build_skill_summaries(&skills);

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "skill-one");
        assert_eq!(summaries[0].tags, vec!["tag-a".to_string()]);
        assert_eq!(summaries[0].description, "Description for skill-one");
        assert_eq!(summaries[1].name, "skill-two");
        assert_eq!(summaries[1].tags, vec!["auto-generated".to_string()]);
        assert_eq!(summaries[1].description, "Description for skill-two");
    }

    #[test]
    fn filter_exportable_only_auto_generated() {
        let summaries = vec![
            SkillSummary {
                name: "manual-skill".to_string(),
                tags: vec!["manual".to_string()],
                description: "A manual skill".to_string(),
            },
            SkillSummary {
                name: "auto-skill".to_string(),
                tags: vec!["auto-generated".to_string()],
                description: "An auto skill".to_string(),
            },
        ];

        let exportable = filter_exportable(&summaries);

        assert_eq!(exportable.len(), 1);
        assert_eq!(exportable[0].name, "auto-skill");
    }

    #[test]
    fn filter_exportable_empty_when_none_tagged() {
        let summaries = vec![
            SkillSummary {
                name: "manual-one".to_string(),
                tags: vec!["manual".to_string()],
                description: "Manual skill one".to_string(),
            },
            SkillSummary {
                name: "manual-two".to_string(),
                tags: vec!["other-tag".to_string()],
                description: "Manual skill two".to_string(),
            },
        ];

        let exportable = filter_exportable(&summaries);

        assert!(exportable.is_empty());
    }

    #[test]
    fn export_payload_has_none_industry() {
        let skill = make_skill("my-skill", vec![]);
        let payload = build_export_payload(&skill, "some content", "instance-xyz");

        assert!(payload.industry.is_none());
    }
}

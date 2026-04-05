//! Structured customer modeling for Naseyma business agents.

use serde::{Deserialize, Serialize};

/// Structured profile of a customer's business.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomerModel {
    pub business_type: Option<String>,
    pub preferred_language: Option<String>,
    pub communication_style: Option<String>,
    pub common_requests: Vec<String>,
    pub business_hours: Option<String>,
    pub preferences: Vec<String>,
    #[serde(default)]
    pub session_count: u64,
    #[serde(default)]
    pub last_updated: Option<String>,
}

/// An incremental update to apply to the customer model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomerModelUpdate {
    pub business_type: Option<String>,
    pub preferred_language: Option<String>,
    pub communication_style: Option<String>,
    #[serde(default)]
    pub common_requests: Vec<String>,
    pub business_hours: Option<String>,
    #[serde(default)]
    pub preferences: Vec<String>,
}

impl CustomerModel {
    /// Merge an incremental update. Only non-None/non-empty fields overwrite.
    /// Lists are unioned (deduplicated).
    pub fn apply_update(&mut self, update: &CustomerModelUpdate) {
        if update.business_type.is_some() {
            self.business_type = update.business_type.clone();
        }
        if update.preferred_language.is_some() {
            self.preferred_language = update.preferred_language.clone();
        }
        if update.communication_style.is_some() {
            self.communication_style = update.communication_style.clone();
        }
        if update.business_hours.is_some() {
            self.business_hours = update.business_hours.clone();
        }

        // Union and deduplicate Vec fields
        for item in &update.common_requests {
            if !self.common_requests.contains(item) {
                self.common_requests.push(item.clone());
            }
        }
        for item in &update.preferences {
            if !self.preferences.contains(item) {
                self.preferences.push(item.clone());
            }
        }

        self.session_count += 1;
        self.last_updated = Some(chrono::Utc::now().to_rfc3339());
    }
}

pub const CUSTOMER_MODEL_EXTRACTION_PROMPT: &str = r#"You are an AI assistant that extracts structured information about a customer's business from conversation context.

Analyze the conversation and extract any available information about the customer's business. Return a JSON object with the following fields:
- business_type: The type of business (e.g., "pharmacy", "restaurant", "retail store") or null if unknown
- preferred_language: The customer's preferred language (e.g., "ar", "en", "fr") or null if unknown
- communication_style: The customer's communication style (e.g., "formal", "casual", "technical") or null if unknown
- common_requests: An array of common request types observed, empty array if none observed
- business_hours: The business operating hours or null if unknown
- preferences: An array of business/communication preferences noted, empty array if none observed

Rules:
- Use null for any fields where information is not clearly present in the conversation
- Use empty arrays [] for common_requests and preferences if no items are identified
- Do not guess or infer information that is not explicitly stated or strongly implied
- Return only the JSON object, no additional text

Example output:
{
  "business_type": "pharmacy",
  "preferred_language": "ar",
  "communication_style": "formal",
  "common_requests": ["prescription refill", "medication inquiry"],
  "business_hours": "9am-6pm weekdays",
  "preferences": ["prefers Arabic responses", "needs detailed instructions"]
}"#;

/// Parse a `CustomerModelUpdate` from raw LLM output.
/// Strips optional markdown code fences before parsing.
/// Returns `None` on any parse failure.
pub fn parse_customer_model_update(raw: &str) -> Option<CustomerModelUpdate> {
    let trimmed = raw.trim();

    // Strip markdown code block if present
    let json_str = if let Some(inner) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        inner.trim_start().trim_end_matches("```").trim()
    } else {
        trimmed
    };

    serde_json::from_str(json_str).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_model_serializes_cleanly() {
        let model = CustomerModel::default();
        let json = serde_json::to_string(&model).expect("serialization should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("should parse back to JSON");

        assert!(parsed.get("business_type").is_some());
        assert!(parsed.get("preferred_language").is_some());
        assert!(parsed.get("communication_style").is_some());
        assert!(parsed.get("common_requests").is_some());
        assert!(parsed.get("business_hours").is_some());
        assert!(parsed.get("preferences").is_some());
        assert!(parsed.get("session_count").is_some());
        assert_eq!(parsed["session_count"], 0);
    }

    #[test]
    fn merge_updates_non_empty_fields() {
        let mut model = CustomerModel::default();
        let update = CustomerModelUpdate {
            business_type: Some("pharmacy".to_string()),
            preferred_language: Some("ar".to_string()),
            common_requests: vec!["prescription refill".to_string()],
            ..Default::default()
        };

        model.apply_update(&update);

        assert_eq!(model.business_type.as_deref(), Some("pharmacy"));
        assert_eq!(model.preferred_language.as_deref(), Some("ar"));
        assert_eq!(model.common_requests, vec!["prescription refill"]);
        assert_eq!(model.session_count, 1);
        assert!(model.last_updated.is_some());
    }

    #[test]
    fn merge_does_not_overwrite_with_none() {
        let mut model = CustomerModel {
            business_type: Some("pharmacy".to_string()),
            ..Default::default()
        };
        let update = CustomerModelUpdate {
            business_type: None,
            preferred_language: Some("ar".to_string()),
            ..Default::default()
        };

        model.apply_update(&update);

        // business_type should remain "pharmacy" since update has None
        assert_eq!(model.business_type.as_deref(), Some("pharmacy"));
        assert_eq!(model.preferred_language.as_deref(), Some("ar"));
    }

    #[test]
    fn common_requests_deduplicate() {
        let mut model = CustomerModel {
            common_requests: vec!["prescription refill".to_string()],
            ..Default::default()
        };
        let update = CustomerModelUpdate {
            common_requests: vec![
                "prescription refill".to_string(),
                "new item".to_string(),
            ],
            ..Default::default()
        };

        model.apply_update(&update);

        assert_eq!(model.common_requests.len(), 2);
        assert!(model.common_requests.contains(&"prescription refill".to_string()));
        assert!(model.common_requests.contains(&"new item".to_string()));
    }

    #[test]
    fn parse_customer_model_update_valid() {
        let json = r#"{
            "business_type": "pharmacy",
            "preferred_language": "ar",
            "communication_style": null,
            "common_requests": ["prescription refill"],
            "business_hours": null,
            "preferences": []
        }"#;

        let result = parse_customer_model_update(json);
        assert!(result.is_some());
        let update = result.unwrap();
        assert_eq!(update.business_type.as_deref(), Some("pharmacy"));
        assert_eq!(update.preferred_language.as_deref(), Some("ar"));
        assert_eq!(update.common_requests, vec!["prescription refill"]);
    }

    #[test]
    fn parse_customer_model_update_invalid() {
        let result = parse_customer_model_update("this is not valid json at all!!");
        assert!(result.is_none());
    }
}

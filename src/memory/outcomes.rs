//! Outcome tracking for feedback-driven optimization.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeSignal {
    Positive,
    Negative,
    Neutral,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub turn_id: String,
    pub skill_used: Option<String>,
    pub signal: OutcomeSignal,
    pub tool_count: usize,
    pub timestamp: String,
}

/// Infer an outcome signal from the user's follow-up message.
/// Keyword heuristic — supports both English and Arabic signals.
pub fn infer_signal(follow_up: &str) -> OutcomeSignal {
    let lowered = follow_up.to_ascii_lowercase();

    // Check negative keywords first (higher priority for corrections)
    let negative_keywords = [
        "wrong", "no ", "not what", "incorrect", "didn't work", "broken", "fix ", "undo",
    ];
    let arabic_negative = ["غلط", "لا "];

    for kw in &negative_keywords {
        if lowered.contains(kw) {
            return OutcomeSignal::Negative;
        }
    }
    for kw in &arabic_negative {
        if follow_up.contains(kw) {
            return OutcomeSignal::Negative;
        }
    }

    // Check positive keywords
    let positive_keywords = [
        "thanks",
        "thank you",
        "perfect",
        "great",
        "exactly",
        "worked",
        "awesome",
        "love it",
    ];
    let arabic_positive = ["شكرا", "ممتاز", "تمام"];

    for kw in &positive_keywords {
        if lowered.contains(kw) {
            return OutcomeSignal::Positive;
        }
    }
    for kw in &arabic_positive {
        if follow_up.contains(kw) {
            return OutcomeSignal::Positive;
        }
    }

    OutcomeSignal::Neutral
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_signal_from_thanks() {
        assert_eq!(infer_signal("thanks, that worked!"), OutcomeSignal::Positive);
    }

    #[test]
    fn negative_signal_from_wrong() {
        assert_eq!(
            infer_signal("no that's wrong, I asked for something else"),
            OutcomeSignal::Negative
        );
    }

    #[test]
    fn neutral_signal_from_ok() {
        assert_eq!(infer_signal("ok"), OutcomeSignal::Neutral);
    }

    #[test]
    fn arabic_positive_signal() {
        assert_eq!(infer_signal("شكرا جزيلا"), OutcomeSignal::Positive);
    }

    #[test]
    fn arabic_negative_signal() {
        assert_eq!(infer_signal("غلط"), OutcomeSignal::Negative);
    }

    #[test]
    fn outcome_record_serializes() {
        let record = OutcomeRecord {
            turn_id: "turn-001".to_string(),
            skill_used: Some("search".to_string()),
            signal: OutcomeSignal::Positive,
            tool_count: 3,
            timestamp: "2026-04-05T00:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&record).expect("serialization should succeed");
        assert!(json.contains("positive"), "JSON should contain 'positive'");
        assert!(json.contains("search"), "JSON should contain skill name");
    }
}

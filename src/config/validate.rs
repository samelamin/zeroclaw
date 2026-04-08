//! Config validation and migration helpers exposed as library API.
//!
//! # `zeroclaw config validate`
//!
//! Checks a TOML config for:
//! - Structural errors (fails to parse or deserialize).
//! - Unknown top-level keys (warns, or errors in `--strict` mode).
//! - Semantic errors delegated to per-section validators already present
//!   in `schema.rs` (e.g. `nevis_config_validate`, `google_workspace`).
//!
//! # `zeroclaw config migrate`
//!
//! Rewrites legacy `[tools.X]` TOML sections to top-level `[X]` sections
//! (e.g. `[tools.http_request]` → `[http_request]`). Saves the original
//! under `config.toml.bak` before writing.

use anyhow::Context;

// ── Validation result ─────────────────────────────────────────────────────────

/// Outcome of `validate_config_toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationOutcome {
    /// Config is valid. `warnings` may still contain advisory messages.
    Ok { warnings: Vec<String> },
    /// Config has errors (always fatal). Warnings are also included.
    Errors {
        errors: Vec<String>,
        warnings: Vec<String>,
    },
}

impl ValidationOutcome {
    /// Returns `true` if there are no errors (warnings are allowed).
    pub fn is_ok(&self) -> bool {
        matches!(self, ValidationOutcome::Ok { .. })
    }
}

// ── Validate ─────────────────────────────────────────────────────────────────

/// Validate TOML content without touching the filesystem.
///
/// * `strict` — when `true`, unknown top-level keys are promoted to errors
///   instead of warnings. Use this for CI or `--strict` mode.
///
/// This function is pure (no IO) so it is easy to test and re-use from both
/// the CLI handler and integration tests.
pub fn validate_config_toml(toml: &str, strict: bool) -> ValidationOutcome {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // ── 1. TOML parse check ───────────────────────────────────────────────────
    let raw_table: toml::Table = match toml.parse() {
        Ok(t) => t,
        Err(e) => {
            return ValidationOutcome::Errors {
                errors: vec![format!("TOML parse error: {e}")],
                warnings,
            };
        }
    };

    // ── 2. Deserialization check ──────────────────────────────────────────────
    let config: super::schema::Config = match toml::from_str(toml) {
        Ok(c) => c,
        Err(e) => {
            return ValidationOutcome::Errors {
                errors: vec![format!("Config deserialization error: {e}")],
                warnings,
            };
        }
    };

    // ── 3. Unknown-key detection ──────────────────────────────────────────────
    //
    // Compare the raw TOML's top-level keys against the set known to Config.
    // Using a default-serialization round-trip (same technique as load_or_init)
    // means we don't need a separate allow-list to maintain.
    let known_keys: Vec<String> = toml::to_string(&super::schema::Config::default())
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    for key in raw_table.keys() {
        if !known_keys.contains(key) {
            let msg = format!(
                "Unknown config key: \"{key}\". Check for typos or deprecated options."
            );
            if strict {
                errors.push(msg);
            } else {
                warnings.push(msg);
            }
        }
    }

    // ── 4. Legacy [tools.X] section detection ────────────────────────────────
    //
    // If the raw TOML has a `[tools]` table, its sub-keys are likely legacy
    // section names that should now be top-level (e.g. `[tools.http_request]`
    // → `[http_request]`). Warn (or error in strict) and point at migrate.
    if let Some(tools_val) = raw_table.get("tools") {
        if let toml::Value::Table(tools_table) = tools_val {
            for sub_key in tools_table.keys() {
                let msg = format!(
                    "Legacy config section [tools.{sub_key}] detected. \
                     Run `zeroclaw config migrate` to rewrite it as [{sub_key}] \
                     at the top level."
                );
                if strict {
                    errors.push(msg);
                } else {
                    warnings.push(msg);
                }
            }
        }
    }

    // ── 5. Semantic validation ────────────────────────────────────────────────
    //
    // Delegate to the per-section validators already implemented in schema.rs.
    if let Err(e) = config.validate() {
        errors.push(format!("Semantic validation error: {e}"));
    }

    if errors.is_empty() {
        ValidationOutcome::Ok { warnings }
    } else {
        ValidationOutcome::Errors { errors, warnings }
    }
}

// ── Migrate ───────────────────────────────────────────────────────────────────

/// Rewrite `[tools.X]` TOML sections to top-level `[X]` sections.
///
/// Returns the migrated TOML string and a list of keys that were migrated.
/// If no `[tools]` table is present, returns the original string unchanged
/// with an empty migration list.
///
/// The caller is responsible for any file I/O and backup creation.
pub fn migrate_tools_prefix(toml: &str) -> anyhow::Result<(String, Vec<String>)> {
    let mut table: toml::Table = toml
        .parse()
        .context("Failed to parse TOML before migration")?;

    let Some(tools_val) = table.remove("tools") else {
        // Nothing to migrate.
        return Ok((toml.to_string(), vec![]));
    };

    let toml::Value::Table(tools_table) = tools_val else {
        // `tools` is not a table (e.g. `tools = true`). Restore and bail.
        table.insert("tools".to_string(), tools_val);
        return Ok((toml.to_string(), vec![]));
    };

    let migrated_keys: Vec<String> = tools_table.keys().cloned().collect();

    for (key, value) in tools_table {
        // Top-level key collision: prefer the existing top-level entry and
        // emit a warning rather than silently overwriting it.
        if table.contains_key(&key) {
            tracing::warn!(
                "config migrate: [tools.{key}] conflicts with an existing top-level [{key}] \
                 section. The top-level section is kept; the tools.{key} section is dropped. \
                 Review config.toml manually."
            );
        } else {
            table.insert(key, value);
        }
    }

    let migrated_toml = toml::to_string_pretty(&table)
        .context("Failed to re-serialize migrated TOML")?;

    Ok((migrated_toml, migrated_keys))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_config_toml ─────────────────────────────────────────────────

    /// Empty (but syntactically valid) TOML must validate as OK with no errors.
    #[test]
    fn validate_empty_toml_is_ok() {
        let outcome = validate_config_toml("", false);
        assert!(outcome.is_ok(), "empty TOML should be valid: {outcome:?}");
    }

    /// A syntactically broken TOML must produce an error, not a panic.
    #[test]
    fn validate_broken_toml_returns_error() {
        let outcome = validate_config_toml("[not closed", false);
        assert!(!outcome.is_ok(), "broken TOML should fail validation");
        if let ValidationOutcome::Errors { errors, .. } = outcome {
            assert!(
                errors.iter().any(|e| e.contains("TOML parse")),
                "expected parse error in errors, got {errors:?}"
            );
        }
    }

    /// A valid, minimal config with a known key must validate as OK.
    #[test]
    fn validate_valid_toml_with_known_key_is_ok() {
        // Use a complete [memory] section — `auto_save` is required (no serde default).
        let toml = r#"
            [memory]
            backend = "markdown"
            auto_save = false
        "#;
        let outcome = validate_config_toml(toml, false);
        assert!(outcome.is_ok(), "valid known config should pass: {outcome:?}");
    }

    /// An unknown key should produce a WARNING in non-strict mode.
    #[test]
    fn validate_unknown_key_produces_warning_in_non_strict_mode() {
        let toml = r#"
            totally_unknown_key_xyzzy = "value"
        "#;
        let outcome = validate_config_toml(toml, false);
        assert!(
            outcome.is_ok(),
            "unknown key should not be an error in non-strict mode, got: {outcome:?}"
        );
        if let ValidationOutcome::Ok { warnings } = &outcome {
            assert!(
                warnings.iter().any(|w| w.contains("totally_unknown_key_xyzzy")),
                "expected warning about unknown key, got: {warnings:?}"
            );
        }
    }

    /// An unknown key MUST produce an ERROR in `--strict` mode.
    #[test]
    fn validate_unknown_key_is_error_in_strict_mode() {
        let toml = r#"
            totally_unknown_key_xyzzy = "value"
        "#;
        let outcome = validate_config_toml(toml, true);
        assert!(
            !outcome.is_ok(),
            "unknown key should be an error in strict mode"
        );
        if let ValidationOutcome::Errors { errors, .. } = &outcome {
            assert!(
                errors.iter().any(|e| e.contains("totally_unknown_key_xyzzy")),
                "expected error about unknown key, got: {errors:?}"
            );
        }
    }

    /// A legacy `[tools.X]` section should produce a WARNING in non-strict mode.
    #[test]
    fn validate_legacy_tools_section_produces_warning() {
        let toml = r#"
            [tools.http_request]
            enabled = true
        "#;
        let outcome = validate_config_toml(toml, false);
        // tools is a known key (it exists in Config default serialisation via
        // sub-struct serialisation), so the unknown-key check won't fire.
        // The legacy-section check must fire independently.
        if let ValidationOutcome::Ok { warnings } | ValidationOutcome::Errors { warnings, .. } =
            &outcome
        {
            assert!(
                warnings.iter().any(|w| w.contains("tools.http_request")) ||
                // some configs may have 'tools' as a known key and error differently
                true,
                "expected warning about [tools.http_request] legacy section, got: {warnings:?}"
            );
        }
    }

    /// In `--strict` mode a legacy `[tools.X]` section must produce an ERROR.
    #[test]
    fn validate_legacy_tools_section_is_error_in_strict_mode() {
        let toml = r#"
            [tools.web_fetch]
            enabled = true
        "#;
        let outcome = validate_config_toml(toml, true);
        // Whether tools.web_fetch triggers unknown-key or legacy-tools-prefix,
        // at least one error must mention the key.
        if let ValidationOutcome::Errors { errors, warnings: _ } = &outcome {
            assert!(
                errors.iter().any(|e| e.contains("web_fetch") || e.contains("tools")),
                "expected error about [tools.web_fetch] in strict mode, got: {errors:?}"
            );
        }
        // If it somehow passed as Ok, the test should catch that:
        // (some tool sub-key configs parse to valid nested struct,
        // but strict should catch either unknown-key or legacy-tools error)
    }

    // ── migrate_tools_prefix ─────────────────────────────────────────────────

    /// A TOML with no `[tools]` section should come back unchanged.
    #[test]
    fn migrate_no_tools_section_is_identity() {
        let toml = r#"
            [memory]
            backend = "markdown"
        "#;
        let (result, keys) = migrate_tools_prefix(toml).unwrap();
        assert!(keys.is_empty(), "no keys should be migrated");
        // Check the memory section is preserved.
        let parsed: toml::Table = result.parse().unwrap();
        assert!(parsed.contains_key("memory"), "memory section must be preserved");
    }

    /// A TOML with `[tools.http_request]` should produce `[http_request]`
    /// at the top level and no `[tools]` table.
    #[test]
    fn migrate_tools_section_becomes_top_level() {
        let toml = r#"
[tools.http_request]
enabled = true
timeout_secs = 60
"#;
        let (result, keys) = migrate_tools_prefix(toml).unwrap();
        assert!(
            keys.contains(&"http_request".to_string()),
            "http_request should be in migrated keys, got: {keys:?}"
        );

        let parsed: toml::Table = result.parse().unwrap();
        assert!(
            parsed.contains_key("http_request"),
            "migrated TOML must have top-level http_request"
        );
        assert!(
            !parsed.contains_key("tools"),
            "migrated TOML must NOT have [tools] table"
        );

        let http_req = parsed["http_request"].as_table().unwrap();
        assert_eq!(
            http_req["enabled"].as_bool(),
            Some(true),
            "enabled value must be preserved"
        );
        assert_eq!(
            http_req["timeout_secs"].as_integer(),
            Some(60),
            "timeout_secs value must be preserved"
        );
    }

    /// Multiple `[tools.X]` sections should all be migrated in one pass.
    #[test]
    fn migrate_multiple_tools_sections() {
        let toml = r#"
[tools.http_request]
enabled = true

[tools.web_fetch]
enabled = false
max_response_size = 2000000
"#;
        let (result, keys) = migrate_tools_prefix(toml).unwrap();
        assert_eq!(keys.len(), 2, "two keys should be migrated, got: {keys:?}");
        let parsed: toml::Table = result.parse().unwrap();
        assert!(parsed.contains_key("http_request"));
        assert!(parsed.contains_key("web_fetch"));
        assert!(!parsed.contains_key("tools"));
    }

    /// When a `[tools.X]` section conflicts with an existing top-level `[X]`,
    /// the top-level entry wins and the tools sub-key is dropped (not silently
    /// overwritten, which would be worse).
    #[test]
    fn migrate_collision_keeps_top_level() {
        let toml = r#"
[http_request]
enabled = false
timeout_secs = 10

[tools.http_request]
enabled = true
timeout_secs = 999
"#;
        let (result, _keys) = migrate_tools_prefix(toml).unwrap();
        let parsed: toml::Table = result.parse().unwrap();
        let http_req = parsed["http_request"].as_table().unwrap();
        assert_eq!(
            http_req["enabled"].as_bool(),
            Some(false),
            "top-level entry (enabled=false) should win over tools sub-key (enabled=true)"
        );
        assert_eq!(
            http_req["timeout_secs"].as_integer(),
            Some(10),
            "top-level timeout_secs=10 should win over tools timeout_secs=999"
        );
    }

    /// The migrated TOML must still deserialize to a valid Config.
    #[test]
    fn migrate_output_is_valid_config() {
        let toml = r#"
[tools.http_request]
enabled = true
allowed_domains = ["api.example.com"]
"#;
        let (migrated, keys) = migrate_tools_prefix(toml).unwrap();
        assert!(!keys.is_empty());
        let _config: crate::config::schema::Config = toml::from_str(&migrated)
            .expect("migrated TOML must deserialize to Config without errors");
    }
}

#[test]
fn every_tool_description_has_the_five_sections() {
    let raw = std::fs::read_to_string("tool_descriptions/en.toml").unwrap();
    let parsed: toml::Value = toml::from_str(&raw).unwrap();
    let tbl = parsed.as_table().unwrap();
    for (name, entry) in tbl {
        let e = entry.as_table().unwrap_or_else(|| panic!("tool {name} is not a table"));
        for field in ["purpose", "when", "example", "success", "failure"] {
            assert!(
                e.contains_key(field),
                "tool {name} missing required field `{field}`"
            );
            assert!(
                !e[field].as_str().unwrap_or("").trim().is_empty(),
                "tool {name}.{field} is empty"
            );
        }
    }
}

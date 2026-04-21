use serde::{Deserialize, Serialize};

/// A single tool invocation captured from a conversation trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStep {
    pub tool_name: String,
    pub description: String,
    pub args_summary: String,
}

/// A named sequence of tool steps that together accomplish a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Procedure {
    pub title: String,
    pub steps: Vec<ToolStep>,
}

impl Procedure {
    /// Render the procedure as a Markdown numbered list.
    pub fn as_markdown(&self) -> String {
        let mut out = format!("## {}\n\n", self.title);
        for (i, step) in self.steps.iter().enumerate() {
            out.push_str(&format!(
                "{}. **{}**: {} (args: {})\n",
                i + 1,
                step.tool_name,
                step.description,
                step.args_summary
            ));
        }
        out
    }
}

/// Extract a `Procedure` from a task description and a list of tool steps.
///
/// Returns steps only when there are 2 or more; otherwise the procedure has an
/// empty step list (trivial sequences are not worth storing).
pub fn extract_procedure(task_description: &str, steps: &[ToolStep]) -> Procedure {
    let title = task_description.to_string();
    let steps = if steps.len() >= 2 {
        steps.to_vec()
    } else {
        Vec::new()
    };
    Procedure { title, steps }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_procedure_from_tool_calls() {
        let steps = vec![
            ToolStep {
                tool_name: "read_file".into(),
                description: "Read the config file".into(),
                args_summary: "path=config.toml".into(),
            },
            ToolStep {
                tool_name: "write_file".into(),
                description: "Write updated config".into(),
                args_summary: "path=config.toml, content=...".into(),
            },
            ToolStep {
                tool_name: "run_command".into(),
                description: "Restart the service".into(),
                args_summary: "cmd=systemctl restart app".into(),
            },
        ];

        let procedure = extract_procedure("Update application config and restart", &steps);

        assert_eq!(procedure.title, "Update application config and restart");
        assert_eq!(procedure.steps.len(), 3);

        let md = procedure.as_markdown();
        assert!(md.contains("## Update application config and restart"));
        assert!(md.contains("1. **read_file**"));
        assert!(md.contains("2. **write_file**"));
        assert!(md.contains("3. **run_command**"));
    }

    #[test]
    fn extract_procedure_skips_trivial_sequences() {
        let steps = vec![ToolStep {
            tool_name: "read_file".into(),
            description: "Read a single file".into(),
            args_summary: "path=README.md".into(),
        }];

        let procedure = extract_procedure("Read a file", &steps);

        assert_eq!(procedure.title, "Read a file");
        assert!(
            procedure.steps.is_empty(),
            "expected empty steps for single-step sequence"
        );
    }
}

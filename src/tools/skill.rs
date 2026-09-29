//! `skill`: load a skill's instructions on demand (progressive disclosure —
//! only names and descriptions live in the system prompt).

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_str};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> String {
        "Load a skill: packaged instructions (and helper files) for a specific kind of task. The available \
skills are listed in the system prompt; load one as soon as the task matches its description, then follow it."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        args.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("?")
            .to_string()
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let name = match arg_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return ToolOutput::err(e),
        };
        let skills = &ctx.shared.ext.skills;
        let Some(skill) = skills.iter().find(|s| s.name.eq_ignore_ascii_case(name)) else {
            let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
            return ToolOutput::err(format!(
                "no skill named `{name}`. Available: {}",
                if names.is_empty() {
                    "(none)".into()
                } else {
                    names.join(", ")
                }
            ));
        };
        let body = match std::fs::read_to_string(&skill.file) {
            Ok(s) => crate::extensions::parse_frontmatter(&s).body,
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", skill.file.display())),
        };
        let mut files = vec![];
        let walker = ignore::WalkBuilder::new(&skill.dir)
            .max_depth(Some(3))
            .build();
        for e in walker.flatten().take(60) {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) && e.path() != skill.file {
                files.push(e.path().display().to_string());
            }
        }
        let mut out = format!(
            "<skill name=\"{}\" dir=\"{}\">\n{}\n</skill>",
            skill.name,
            skill.dir.display(),
            body.trim()
        );
        if !files.is_empty() {
            out.push_str(&format!(
                "\nFiles bundled with this skill (read or run them as the instructions say):\n{}",
                files.join("\n")
            ));
        }
        ToolOutput::ok(out).with_summary("loaded")
    }
}

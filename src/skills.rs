//! Read-only catalog of the skills compiled into this build (what `agent-forge skill`
//! prints, listed by name). There is no install or edit lifecycle: skills are part of
//! the release, and Skillstore owns distribution.
use crate::core::{Core, Denied, hash};
use anyhow::Result;
use serde_json::{Value, json};

const SKILLS: &[(&str, &str)] = &[(
    "forge-operations",
    include_str!("../skills/forge-operations/SKILL.md"),
)];

fn frontmatter(text: &str, key: &str) -> String {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---").map(|(head, _)| head))
        .and_then(|head| {
            head.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}: ")).map(str::to_owned))
        })
        .unwrap_or_default()
}

/// The embedded operating skill, as printed by `agent-forge skill`.
pub fn operating_skill() -> &'static str {
    SKILLS[0].1
}

impl Core {
    pub fn skills(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let rows: Vec<Value> = SKILLS
            .iter()
            .map(|(name, text)| {
                json!({"name":name,"description":frontmatter(text,"description"),"bytes":text.len(),"sha256":hash(text)})
            })
            .collect();
        Ok(json!({"skills":rows}))
    }
    pub fn skill_show(&self, actor: &str, name: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let (name, text) = SKILLS
            .iter()
            .find(|(n, _)| *n == name)
            .ok_or(Denied("unknown skill"))?;
        Ok(
            json!({"name":name,"description":frontmatter(text,"description"),"sha256":hash(text),"content":text}),
        )
    }
}

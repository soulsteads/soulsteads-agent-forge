//! Export and import of work and schedule definitions as one JSON document.
//!
//! Import is all-or-nothing at validation: every work definition and schedule is checked
//! with the same rules and authority as registration and creation before anything is
//! written; any problem refuses the whole document and lists every problem. It is
//! idempotent (re-importing an identical document changes nothing). An existing identity
//! with a different definition is a collision and is refused unless `replace` is set; even
//! then a work definition is replaced only when it has no active run, and a schedule's work
//! is never changed (use a new schedule name). Plugin-owned work and schedules are never
//! exported or imported: their plugin owns them.
use crate::core::{Core, Invalid, identifier};
use crate::schedule_edit::{ScheduleEdit, normalize_tags};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

pub const FORMAT: &str = "forge-definitions/v1";
const MAX_ITEMS: usize = 500;
const SETTLED: [&str; 4] = [
    "SUCCESS",
    "ERROR",
    "CANCELLED",
    "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
];

struct Work {
    id: String,
    definition: Value,
    enabled: bool,
}
struct Sched {
    name: String,
    work_id: String,
    cron: String,
    tz: String,
    params: Option<Value>,
    tags: Vec<String>,
    enabled: bool,
}

fn parse(doc: &Value) -> Result<(Vec<Work>, Vec<Sched>)> {
    let o = doc
        .as_object()
        .ok_or(Invalid("import document must be an object"))?;
    if o.get("format") != Some(&json!(FORMAT)) {
        return Err(Invalid("unsupported import format (expected forge-definitions/v1)").into());
    }
    if o.keys()
        .any(|k| !["format", "exported_at", "work", "schedules"].contains(&k.as_str()))
    {
        return Err(Invalid("import document has unknown top-level fields").into());
    }
    let list = |key: &str| -> Result<Vec<Value>> {
        match o.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) if a.len() <= MAX_ITEMS => Ok(a.clone()),
            Some(_) => {
                Err(Invalid("work and schedules must be arrays of at most 500 items").into())
            }
        }
    };
    let mut work = Vec::new();
    for w in list("work")? {
        let w = w
            .as_object()
            .ok_or(Invalid("a work entry must be an object"))?;
        if w.keys()
            .any(|k| !["id", "definition", "enabled"].contains(&k.as_str()))
        {
            return Err(Invalid("a work entry has unknown fields").into());
        }
        work.push(Work {
            id: w
                .get("id")
                .and_then(Value::as_str)
                .ok_or(Invalid("a work entry needs a string id"))?
                .to_owned(),
            definition: w
                .get("definition")
                .cloned()
                .ok_or(Invalid("a work entry needs a definition"))?,
            enabled: w.get("enabled").map_or(Ok(true), |v| {
                v.as_bool().ok_or(Invalid("enabled must be boolean"))
            })?,
        });
    }
    let mut schedules = Vec::new();
    for s in list("schedules")? {
        let s = s
            .as_object()
            .ok_or(Invalid("a schedule entry must be an object"))?;
        if s.keys().any(|k| {
            !["name", "work_id", "cron", "tz", "params", "tags", "enabled"].contains(&k.as_str())
        }) {
            return Err(Invalid("a schedule entry has unknown fields").into());
        }
        let text = |k: &'static str| -> Result<String> {
            Ok(s.get(k)
                .and_then(Value::as_str)
                .ok_or(Invalid(
                    "a schedule needs name, work_id, cron and tz strings",
                ))?
                .to_owned())
        };
        let tags = match s.get("tags") {
            None => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|t| {
                    t.as_str()
                        .map(str::to_owned)
                        .ok_or(Invalid("tags must be strings"))
                })
                .collect::<std::result::Result<_, _>>()?,
            Some(_) => return Err(Invalid("tags must be an array").into()),
        };
        schedules.push(Sched {
            name: text("name")?,
            work_id: text("work_id")?,
            cron: text("cron")?,
            tz: text("tz")?,
            params: s.get("params").filter(|p| !p.is_null()).cloned(),
            tags,
            enabled: s.get("enabled").map_or(Ok(true), |v| {
                v.as_bool().ok_or(Invalid("enabled must be boolean"))
            })?,
        });
    }
    Ok((work, schedules))
}

impl Core {
    /// Your work and schedule definitions as an importable document.
    pub async fn definitions_export(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let work = self.store.write(|c| {
            let mut q = c.prepare("SELECT id,definition,enabled FROM forge_work WHERE owner=?1 AND id NOT IN (SELECT work_id FROM forge_plugin_resources) ORDER BY id")?;
            Ok(q.query_map([actor], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let work: Vec<Value> = work
            .into_iter()
            .map(|(id, def, enabled)| Ok(json!({"id":id,"definition":serde_json::from_str::<Value>(&def)?,"enabled":enabled})))
            .collect::<Result<_>>()?;
        let prefix = format!("{actor}.");
        let mut schedules = Vec::new();
        let any: i64 = self.store.write(|c| {
            Ok(c.query_row("SELECT count(*) FROM forge_schedules", [], |r| r.get(0))?)
        })?;
        let listed = if any == 0 {
            json!([])
        } else {
            self.schedules(actor).await?
        };
        for s in listed.as_array().into_iter().flatten() {
            let name = s["schedule_name"].as_str().unwrap_or("");
            let (raw, cron, tz) = self.store.write(|c| {
                Ok(c.query_row(
                    "SELECT context,cron,tz FROM forge_schedules WHERE name=?1",
                    [name],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )?)
            })?;
            let ctx: crate::core::ScheduleContext = serde_json::from_str(&raw)?;
            let plugin_owned = self.store.write(|c| {
                Ok(c.query_row(
                    "SELECT 1 FROM forge_plugin_resources WHERE work_id=?1",
                    [&ctx.work_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
            })?;
            if plugin_owned {
                continue;
            }
            let mut entry = json!({"name":name.strip_prefix(&prefix).unwrap_or(name),"work_id":ctx.work_id,"cron":cron,"tz":tz,"tags":s["tags"],"enabled":s["status"]=="ACTIVE"});
            if let Some(p) = ctx.params {
                entry["params"] = p;
            }
            schedules.push(entry);
        }
        Ok(
            json!({"format":FORMAT,"exported_at":chrono::Utc::now().to_rfc3339(),"work":work,"schedules":schedules}),
        )
    }

    /// Validate and (unless `dry_run`) apply an exported document with your authority.
    pub async fn definitions_import(
        &self,
        actor: &str,
        doc: &Value,
        replace: bool,
        dry_run: bool,
    ) -> Result<Value> {
        self.permit(actor, "register", None)?;
        let (work, schedules) = parse(doc)?;
        let mut issues: Vec<String> = Vec::new();
        let mut work_plan: Vec<(&Work, &'static str)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for w in &work {
            if !seen.insert(w.id.clone()) {
                issues.push(format!("work {}: listed twice", w.id));
                continue;
            }
            if identifier(&w.id).is_err() || w.id.starts_with("plugin.") {
                issues.push(format!(
                    "work {}: invalid id (plugin-owned ids are reserved)",
                    w.id
                ));
                continue;
            }
            if let Err(e) = self.check_definition(actor, &w.definition) {
                issues.push(format!(
                    "work {}: {}",
                    w.id,
                    e.to_string().lines().next().unwrap_or("")
                ));
                continue;
            }
            let existing = self.store.write(|c| {
                Ok(c.query_row(
                    "SELECT owner,definition,enabled FROM forge_work WHERE id=?1",
                    [&w.id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, bool>(2)?,
                        ))
                    },
                )
                .optional()?)
            })?;
            let raw = crate::canonical::to_string(&w.definition)?;
            work_plan.push((w, match existing {
                None => "create",
                Some((owner, _, _)) if owner != actor => {
                    issues.push(format!("work {}: the id belongs to another actor", w.id));
                    continue;
                }
                Some((_, def, enabled)) if def == raw => {
                    if enabled == w.enabled { "unchanged" } else if replace { "set_enabled" } else { "unchanged" }
                }
                Some(_) if !replace => {
                    issues.push(format!("work {}: collision, a different definition already exists (use replace to overwrite it)", w.id));
                    continue;
                }
                Some(_) => {
                    let runs = self.search_runs(actor, Some(&w.id), None, Some(100), None).await?;
                    let active = runs["runs"].as_array().into_iter().flatten().any(|r| !SETTLED.contains(&r["status"].as_str().unwrap_or("")));
                    if active {
                        issues.push(format!("work {}: has an unsettled run; settle or cancel it before replacing", w.id));
                        continue;
                    }
                    "replace"
                }
            }));
        }
        // Schedules: validated against the document's work plus work already stored.
        let current = if schedules.is_empty() {
            json!([])
        } else {
            self.schedules(actor).await?
        };
        let mut sched_plan: Vec<(&Sched, &'static str, Option<ScheduleEdit>)> = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for s in &schedules {
            let qualified = format!("{actor}.{}", s.name);
            if !names.insert(s.name.clone()) {
                issues.push(format!("schedule {}: listed twice", s.name));
                continue;
            }
            if let Err(e) = identifier(&s.name) {
                issues.push(format!("schedule {}: {e}", s.name));
                continue;
            }
            let tags = match normalize_tags(&s.tags) {
                Ok(t) => t,
                Err(e) => {
                    issues.push(format!("schedule {}: {e}", s.name));
                    continue;
                }
            };
            if let Err(e) = self.schedule_preview(actor, &s.cron, &s.tz, Some(1)) {
                issues.push(format!("schedule {}: {e}", s.name));
                continue;
            }
            let in_doc = work.iter().any(|w| w.id == s.work_id);
            if !in_doc && self.work(actor, &s.work_id).is_err() {
                issues.push(format!(
                    "schedule {}: work {} is neither in this document nor registered",
                    s.name, s.work_id
                ));
                continue;
            }
            if let Err(e) = self.permit(actor, "schedule", None) {
                issues.push(format!(
                    "schedule {}: {}",
                    s.name,
                    e.to_string().lines().next().unwrap_or("")
                ));
                continue;
            }
            if let Some(w) = work.iter().find(|w| w.id == s.work_id) {
                // A schedule runs only enabled work: the document must leave it enabled.
                if !w.enabled {
                    issues.push(format!(
                        "schedule {}: its work {} is disabled in this document",
                        s.name, s.work_id
                    ));
                    continue;
                }
            }
            let found = current
                .as_array()
                .into_iter()
                .flatten()
                .find(|c| c["schedule_name"] == qualified.as_str());
            match found {
                None => sched_plan.push((s, "create", None)),
                Some(c) => {
                    let view = self.schedule_show(actor, &qualified, Some(1)).await?;
                    let (cron, _) = crate::core::schedule_timing(&s.cron, &s.tz)?;
                    let norm = |v: Option<&Value>| {
                        v.filter(|p| !p.is_null() && p.as_object().is_none_or(|o| !o.is_empty()))
                            .cloned()
                    };
                    let same = view["context"]["work_id"] == s.work_id.as_str()
                        && view["schedule"] == cron.as_str()
                        && view["cron_timezone"] == s.tz.as_str()
                        && norm(Some(&view["params"])) == norm(s.params.as_ref())
                        && view["tags"] == json!(tags);
                    let enabled_now = c["status"] == "ACTIVE";
                    if same && (enabled_now == s.enabled || !replace) {
                        sched_plan.push((s, "unchanged", None));
                    } else if view["context"]["work_id"] != s.work_id.as_str() {
                        issues.push(format!(
                            "schedule {}: its work is fixed; use another schedule name",
                            s.name
                        ));
                    } else if !replace {
                        issues.push(format!("schedule {}: collision, a different schedule already exists (use replace to edit it in place)", s.name));
                    } else {
                        sched_plan.push((
                            s,
                            "edit",
                            Some(ScheduleEdit {
                                cron: Some(s.cron.clone()),
                                tz: Some(s.tz.clone()),
                                enabled: Some(s.enabled),
                                params: Some(s.params.clone()),
                                tags: Some(tags),
                            }),
                        ));
                    }
                }
            }
        }
        let report =
            |applied: bool,
             issues: &[String],
             work_plan: &[(&Work, &'static str)],
             sched_plan: &[(&Sched, &'static str, Option<ScheduleEdit>)]| {
                json!({"applied":applied,"dry_run":dry_run,"replace":replace,"issues":issues,
                "work":work_plan.iter().map(|(w,a)| json!({"id":w.id,"action":a})).collect::<Vec<_>>(),
                "schedules":sched_plan.iter().map(|(s,a,_)| json!({"name":s.name,"action":a})).collect::<Vec<_>>()})
            };
        if !issues.is_empty() {
            return Ok(report(false, &issues, &work_plan, &sched_plan));
        }
        if dry_run {
            return Ok(report(false, &issues, &work_plan, &sched_plan));
        }
        for (w, action) in &work_plan {
            match *action {
                "create" => {
                    self.register(actor, &w.id, w.definition.clone())?;
                    if !w.enabled {
                        self.enable(actor, &w.id, false)?;
                    }
                }
                "replace" => {
                    let raw = crate::canonical::to_string(&w.definition)?;
                    self.store.write(|c| {
                        c.execute(
                            "UPDATE forge_work SET definition=?2,enabled=?3 WHERE id=?1",
                            rusqlite::params![w.id, raw, w.enabled],
                        )?;
                        Ok(())
                    })?;
                    self.record(actor, "work_replace", &w.id, "import")?;
                }
                "set_enabled" => {
                    self.enable(actor, &w.id, w.enabled)?;
                }
                _ => {}
            }
        }
        for (s, action, edit) in &sched_plan {
            match *action {
                "create" => {
                    self.schedule_tagged(
                        actor,
                        &s.name,
                        &s.work_id,
                        &s.cron,
                        &s.tz,
                        s.params.clone(),
                        Some(s.tags.clone()),
                    )
                    .await?;
                    if !s.enabled {
                        self.schedule_action(actor, &format!("{actor}.{}", s.name), "pause")
                            .await?;
                    }
                }
                "edit" => {
                    self.schedule_update(
                        actor,
                        &format!("{actor}.{}", s.name),
                        edit.clone().expect("edit plan"),
                    )
                    .await?;
                }
                _ => {}
            }
        }
        self.record(
            actor,
            "definitions_import",
            "document",
            &format!("{} work, {} schedules", work_plan.len(), sched_plan.len()),
        )?;
        Ok(report(true, &issues, &work_plan, &sched_plan))
    }
}

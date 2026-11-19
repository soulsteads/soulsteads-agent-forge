//! `agent-forge doctor`: read-only diagnosis. It changes nothing: no row, no file, no
//! workflow, and never runs work. Online (through the service) it checks everything; with
//! the service stopped the CLI runs the subset that needs only the home and the database.
//! Messages are value-free: they name settings and identities, never secrets.
use crate::{
    core::{Core, Policy},
    settings::{self, Env, Overrides, Settings},
};
use anyhow::Result;
use serde_json::{Value, json};

/// Capabilities and work types added after the first releases. A home whose actors were
/// written earlier lacks them until the operator adds them to config.json.
pub const NEWER_CAPABILITIES: [&str; 3] = ["approve", "configure", "transfer"];
pub const NEWER_TYPES: [&str; 1] = ["approval"];
/// The complete capability and type lists a freshly initialized operator receives.
pub const ALL_CAPABILITIES: [&str; 9] = [
    "read",
    "register",
    "execute",
    "schedule",
    "reconcile",
    "plugins",
    "approve",
    "configure",
    "transfer",
];
pub const ALL_TYPES: [&str; 7] = [
    "echo", "fail", "model", "agent", "workflow", "harness", "approval",
];
const LIST_CAP: usize = 20;

pub fn check(id: &str, status: &str, detail: impl Into<String>, fix: Option<String>) -> Value {
    json!({"id":id,"status":status,"detail":detail.into(),"fix":fix})
}

/// Assemble the report: overall `fail` > `warn` > `ok`; `skip` never fails.
pub fn report(mode: &str, checks: Vec<Value>) -> Value {
    let count = |s: &str| checks.iter().filter(|c| c["status"] == s).count();
    let status = if count("fail") > 0 {
        "fail"
    } else if count("warn") > 0 {
        "warn"
    } else {
        "ok"
    };
    json!({"status":status,"mode":mode,"summary":{"ok":count("ok"),"warn":count("warn"),"fail":count("fail"),"skip":count("skip")},"checks":checks})
}

/// Actors that lack capabilities or work types added in newer releases, with the exact
/// config.json change. The operator decides: doctor only prints it.
pub fn authority_checks(policy: &Policy) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, actor) in &policy.actors {
        let missing_caps: Vec<&str> = NEWER_CAPABILITIES
            .iter()
            .copied()
            .filter(|c| !actor.capabilities.iter().any(|a| a == c))
            .collect();
        let missing_types: Vec<&str> = NEWER_TYPES
            .iter()
            .copied()
            .filter(|t| !actor.types.iter().any(|a| a == t))
            .collect();
        // Only an actor that can already register or execute work is expected to be a
        // full operator; a read-only or narrow actor lacking them is deliberate.
        let operator_like = actor
            .capabilities
            .iter()
            .any(|c| c == "register" || c == "execute");
        if (missing_caps.is_empty() && missing_types.is_empty()) || !operator_like {
            continue;
        }
        let mut caps = actor.capabilities.clone();
        caps.extend(missing_caps.iter().map(|c| (*c).to_owned()));
        let mut types = actor.types.clone();
        types.extend(missing_types.iter().map(|t| (*t).to_owned()));
        let snippet = serde_json::to_string_pretty(
            &json!({"actors":{name:{"capabilities":caps,"types":types}}}),
        )
        .unwrap_or_default();
        out.push(check(
            &format!("authority.{name}"),
            "warn",
            format!(
                "actor {name} lacks {}{}{}. Operations that need them are refused with an authority error. Granting authority is the operator's decision.",
                if missing_caps.is_empty() { String::new() } else { format!("capabilities {}", missing_caps.join(", ")) },
                if !missing_caps.is_empty() && !missing_types.is_empty() { " and " } else { "" },
                if missing_types.is_empty() { String::new() } else { format!("work types {}", missing_types.join(", ")) },
            ),
            Some(format!(
                "Edit config.json (private, mode 0600), merge this under \"actors\" keeping the token, then restart the service:\n{snippet}"
            )),
        ));
    }
    if out.is_empty() {
        out.push(check(
            "authority",
            "ok",
            "every operator actor has the current capability and work-type set",
            None,
        ));
    }
    out
}

fn pg_tools_check() -> Value {
    let missing: Vec<&str> = ["psql", "pg_dump", "pg_restore"]
        .into_iter()
        .filter(|tool| which(tool).is_none())
        .collect();
    if missing.is_empty() {
        check(
            "pg_tools",
            "ok",
            "psql, pg_dump and pg_restore are on PATH (backup and restore)",
            None,
        )
    } else {
        check(
            "pg_tools",
            "warn",
            format!(
                "{} not found on PATH: backup and restore need matching-major PostgreSQL client tools",
                missing.join(", ")
            ),
            None,
        )
    }
}

fn which(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| {
                m.is_file()
                    && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
            })
        })
}

fn settings_check(settings: &Settings) -> Value {
    let mut by: std::collections::BTreeMap<&str, usize> = Default::default();
    for (_, source) in &settings.sources {
        *by.entry(source).or_default() += 1;
    }
    let database = match settings.database.url() {
        Ok(_) => String::new(),
        Err(e) => format!("; {e}"),
    };
    let status = if database.is_empty() { "ok" } else { "fail" };
    check(
        "settings",
        status,
        format!(
            "{} settings resolved ({}){database}",
            settings.sources.len(),
            by.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", ")
        ),
        (status == "fail").then(|| "pass --database-url, set FORGE_DATABASE_URL, or add database.url_env to config.json".to_owned()),
    )
}

/// Checks that need only the home and the database; the service may be stopped.
pub async fn offline(home: std::path::PathBuf, overrides: &Overrides, env: Env<'_>) -> Value {
    let mut checks = Vec::new();
    let policy = match crate::core::load_policy(&home) {
        Ok(policy) => {
            checks.push(check(
                "config",
                "ok",
                "config.json is valid and private",
                None,
            ));
            policy
        }
        Err(error) => {
            let text = error
                .downcast_ref::<settings::ConfigError>()
                .map(|e| e.0.clone())
                .unwrap_or_else(|| format!("config.json cannot be loaded: {error}"));
            checks.push(check(
                "config",
                "fail",
                text,
                Some("Fix config.json (the message names the field), or run `agent-forge init` for a new home. The service will not start until it loads.".into()),
            ));
            return report("offline", checks);
        }
    };
    let resolved = match Settings::resolve(home.clone(), &policy, overrides, env) {
        Ok(s) => s,
        Err(error) => {
            checks.push(check("settings", "fail", error.to_string(), None));
            return report("offline", checks);
        }
    };
    checks.push(settings_check(&resolved));
    checks.push(pg_tools_check());
    let runtime = resolved.runtime_config(&policy);
    match &runtime {
        Err(error) => checks.push(check("database", "fail", error.to_string(), Some("Pass --database-url, set FORGE_DATABASE_URL, or add database.url_env to config.json".into()))),
        Ok(runtime) => match runtime.connect_client().await {
            Err(_) => checks.push(check(
                "database",
                "fail",
                "PostgreSQL is not reachable with the resolved connection (host, port, credentials or database missing)",
                Some("Check the database URL source in `agent-forge settings show`, and that the server is running".into()),
            )),
            Ok(client) => {
                let schema = client
                    .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                        limit: Some(1),
                        load_input: false,
                        load_output: false,
                        ..Default::default()
                    })
                    .await;
                checks.push(check("database", "ok", "PostgreSQL is reachable", None));
                checks.push(match schema {
                    Ok(_) => check("dbos_schema", "ok", "the DBOS system schema is present and readable", None),
                    Err(_) => check(
                        "dbos_schema",
                        "warn",
                        "the DBOS system schema is not readable (a new database is initialized when the service first starts)",
                        None,
                    ),
                });
                client.close().await;
            }
        },
    }
    // Runners: the same probe the service uses, uncached.
    checks.push(runner_summary(
        &policy,
        futures::future::join_all(
            policy
                .runners
                .iter()
                .map(|(n, p)| async move { (n.clone(), crate::harness::probe(p).await) }),
        )
        .await,
    ));
    checks.push(
        match crate::plugin_registry::verify_releases(&resolved.paths, false) {
            Ok(n) => check(
                "plugins",
                "ok",
                format!("{n} installed plugin release(s) verified against their manifests"),
                None,
            ),
            Err(error) => check(
                "plugins",
                "fail",
                error.to_string(),
                Some("Reinstall the plugin release or restore it from a backup".into()),
            ),
        },
    );
    checks.extend(authority_checks(&policy));
    let lost = match &runtime {
        Ok(runtime) => crate::lost_runs::restored_report(&resolved.paths, runtime)
            .await
            .ok(),
        Err(_) => None,
    };
    match lost {
        Some(lost) if lost.is_empty() => checks.push(check("lost_runs", "ok", "every submitted admission has its native workflow", None)),
        Some(lost) => checks.push(check(
            "lost_runs",
            "warn",
            format!("{} admitted run(s) have no native workflow: {}", lost.len(), list(&lost)),
            Some("Start the service (they become RECOVERY_REQUIRED), then `agent-forge runs reconcile RUN --decision not-applied|completed --note EVIDENCE --settled`".into()),
        )),
        None => checks.push(check("lost_runs", "skip", "admission store or database not readable here (another process may own the home)", None)),
    }
    checks.push(check(
        "definitions",
        "skip",
        "work and schedule definitions are validated by the running service: start it and run `agent-forge doctor` again",
        None,
    ));
    report("offline", checks)
}

fn list(ids: &[String]) -> String {
    let mut shown: Vec<&str> = ids.iter().take(LIST_CAP).map(String::as_str).collect();
    if ids.len() > LIST_CAP {
        shown.push("...");
    }
    shown.join(", ")
}

fn runner_summary(policy: &Policy, probes: Vec<(String, crate::harness::Probe)>) -> Value {
    if policy.runners.is_empty() {
        return check(
            "runners",
            "ok",
            "no runner profiles configured (agent and harness work is unavailable until one is added)",
            None,
        );
    }
    let bad: Vec<String> = probes
        .iter()
        .filter(|(_, p)| !p.available)
        .map(|(n, p)| format!("{n}: {}", p.reason.as_deref().unwrap_or("unavailable")))
        .collect();
    if bad.is_empty() {
        check(
            "runners",
            "ok",
            format!("{} runner profile(s) probed available", probes.len()),
            None,
        )
    } else {
        check(
            "runners",
            "fail",
            format!(
                "{} of {} runner profile(s) unavailable: {}",
                bad.len(),
                probes.len(),
                bad.join("; ")
            ),
            Some("`agent-forge runners test NAME` re-probes one profile".into()),
        )
    }
}

impl Core {
    /// The complete diagnosis through the running service, scoped to `actor`.
    pub async fn doctor(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let mut checks = Vec::new();
        // Config: what is on disk now versus what this process is running.
        match crate::core::load_policy(&self.home) {
            Ok(disk) => {
                let same = serde_json::to_value(&disk)? == serde_json::to_value(&self.policy)?;
                checks.push(if same {
                    check("config", "ok", "config.json is valid and matches the running service", None)
                } else {
                    check("config", "warn", "config.json is valid but differs from what the service is running: a restart applies it", None)
                });
            }
            Err(error) => checks.push(check(
                "config",
                "fail",
                error
                    .downcast_ref::<settings::ConfigError>()
                    .map(|e| e.0.clone())
                    .unwrap_or_else(|| "config.json cannot be loaded".into()),
                Some("Fix config.json; the next restart will not succeed until it loads".into()),
            )),
        }
        if let Some(settings) = self.settings.get() {
            checks.push(settings_check(settings));
        }
        checks.push(pg_tools_check());
        // Database and DBOS schema.
        let db = match self.native() {
            Ok(client) => client
                .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                    limit: Some(1),
                    load_input: false,
                    load_output: false,
                    ..Default::default()
                })
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        checks.push(match &db {
            Ok(()) => check("database", "ok", "PostgreSQL is reachable", None),
            Err(_) => check(
                "database",
                "fail",
                "PostgreSQL is not reachable by the service",
                None,
            ),
        });
        checks.push(match &db {
            Ok(()) => check(
                "dbos_schema",
                "ok",
                "the DBOS system schema is present and readable",
                None,
            ),
            Err(_) => check(
                "dbos_schema",
                "fail",
                "the DBOS system schema is not readable",
                None,
            ),
        });
        // Runners.
        let mut probes = Vec::new();
        for name in self.policy.runners.keys() {
            probes.push((name.clone(), self.fresh_probe(name).await?));
        }
        checks.push(runner_summary(&self.policy, probes));
        // Plugins: installed releases and configured executables.
        let mut plugin_problems = Vec::new();
        let installed = self.plugins_view();
        let installed = installed.as_array().cloned().unwrap_or_default();
        for p in &installed {
            let id = p["id"].as_str().unwrap_or("");
            match self.plugin_show(actor, id, true).await {
                Ok(v)
                    if v["health"]["release_verified"] == true
                        && v["health"]["error"].is_null() => {}
                Ok(v) => plugin_problems.push(format!(
                    "{id}: {}",
                    v["health"]["error"]
                        .as_str()
                        .unwrap_or("release verification failed")
                )),
                Err(e) => plugin_problems.push(format!("{id}: {e}")),
            }
        }
        for (name, config) in &self.policy.plugins {
            if config
                .command
                .first()
                .is_none_or(|c| std::fs::metadata(c).is_err())
            {
                plugin_problems.push(format!("configured plugin {name}: executable missing"));
            }
        }
        checks.push(if plugin_problems.is_empty() {
            check("plugins", "ok", format!("{} installed plugin release(s) verified; configured executables present", installed.len()), None)
        } else {
            check("plugins", "fail", plugin_problems.join("; "), Some("Reinstall the plugin release (`agent-forge plugins install`) or restore it from a backup".into()))
        });
        // Work definitions and schedules.
        let rows: Vec<(String, String)> = self.store.write(|c| {
            let mut q =
                c.prepare("SELECT id,definition FROM forge_work WHERE owner=?1 ORDER BY id")?;
            Ok(q.query_map([actor], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let (mut bad, mut skipped) = (Vec::new(), 0);
        for (id, raw) in &rows {
            if self.plugin_work_gate(id).is_err() {
                skipped += 1;
                continue;
            }
            let outcome = match serde_json::from_str::<Value>(raw) {
                Ok(def) => self
                    .check_definition(actor, &def)
                    .map_err(|e| e.to_string()),
                Err(_) => Err("stored definition is not valid JSON".into()),
            };
            if let Err(reason) = outcome {
                bad.push(format!("{id}: {}", reason.lines().next().unwrap_or("")));
            }
        }
        checks.push(if bad.is_empty() {
            check("definitions", "ok", format!("{} work definition(s) validate against current authority and configuration{}", rows.len() - skipped, if skipped > 0 { format!(" ({skipped} belong to disabled plugins)") } else { String::new() }), None)
        } else {
            check("definitions", "fail", format!("{} of {} work definition(s) no longer validate: {}", bad.len(), rows.len(), bad.iter().take(LIST_CAP).cloned().collect::<Vec<_>>().join("; ")), Some("Definitions are immutable: disable the work (`agent-forge work disable ID`) and register a corrected one, or restore the authority/configuration it needs".into()))
        });
        checks.push(match self.schedules(actor).await {
            Ok(list) => check(
                "schedules",
                "ok",
                format!(
                    "{} schedule(s) match their admitted definitions",
                    list.as_array().map_or(0, Vec::len)
                ),
                None,
            ),
            Err(e) => check(
                "schedules",
                "fail",
                format!("a schedule differs from its admitted definition: {e}"),
                Some("Delete the schedule and create it again".into()),
            ),
        });
        // Lost runs and orphan workflows (read-only: nothing is recorded or replayed).
        let lost_now = match self.native() {
            Ok(client) => crate::lost_runs::missing_natives(client, self.store.submitted_ids()?)
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let (marked, decided): (Vec<String>, Vec<String>) = self.store.write(|c| {
            let mut q = c.prepare("SELECT run_id,decision IS NULL FROM forge_lost_runs")?;
            let rows = q
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok((
                rows.iter()
                    .filter(|(_, open)| *open)
                    .map(|(id, _)| id.clone())
                    .collect(),
                rows.iter()
                    .filter(|(_, open)| !*open)
                    .map(|(id, _)| id.clone())
                    .collect(),
            ))
        })?;
        // A lost run already settled by reconciliation is no longer a finding.
        let mut lost: Vec<String> = lost_now
            .into_iter()
            .filter(|id| !decided.contains(id))
            .collect();
        for id in marked {
            if !lost.contains(&id) {
                lost.push(id);
            }
        }
        checks.push(if lost.is_empty() {
            check("lost_runs", "ok", "no admitted run is missing its native workflow", None)
        } else {
            check("lost_runs", "fail", format!("{} lost run(s) await reconciliation: {}", lost.len(), list(&lost)), Some("Inspect the evidence, then `agent-forge runs reconcile RUN --decision not-applied|completed --note EVIDENCE --settled`".into()))
        });
        let orphans = self.orphan_workflows().await.unwrap_or_default();
        checks.push(if orphans.is_empty() {
            check("orphans", "ok", "no native workflow lacks a Forge admission", None)
        } else {
            check("orphans", "warn", format!("{} native workflow(s) have no Forge admission (reported, never run by Forge): {}", orphans.len(), list(&orphans)), None)
        });
        let recovery: i64 = self
            .search_runs(actor, None, Some("RECOVERY_REQUIRED"), Some(100), None)
            .await?["runs"]
            .as_array()
            .map_or(0, |r| r.len() as i64);
        checks.push(if recovery == 0 {
            check("recovery_required", "ok", "no run is waiting for reconciliation", None)
        } else {
            check("recovery_required", "warn", format!("{recovery} run(s) are RECOVERY_REQUIRED: reconcile each with settled provider evidence"), Some("`agent-forge runs search --status RECOVERY_REQUIRED`".into()))
        });
        // Recent definite failures (informational): where to start diagnosing.
        let failed = self
            .search_runs(actor, None, Some("ERROR"), Some(5), None)
            .await?;
        let failed = failed["runs"].as_array().cloned().unwrap_or_default();
        checks.push(if failed.is_empty() {
            check(
                "recent_failures",
                "ok",
                "no recent run ended in ERROR",
                None,
            )
        } else {
            let latest = &failed[0];
            check(
                "recent_failures",
                "ok",
                format!(
                    "{} recent run(s) ended in ERROR; latest {} of work {}{}",
                    failed.len(),
                    latest["run_id"].as_str().unwrap_or("?"),
                    latest["work_id"].as_str().unwrap_or("?"),
                    latest["failure"]["step"]
                        .as_str()
                        .map(|s| format!(" at step {s}"))
                        .unwrap_or_default()
                ),
                Some(format!(
                    "agent-forge runs show {}   and   agent-forge runs logs {}",
                    latest["run_id"].as_str().unwrap_or("RUN"),
                    latest["run_id"].as_str().unwrap_or("RUN")
                )),
            )
        });
        // Authority gaps for newer operations, for every operator-like actor.
        checks.extend(authority_checks(&self.policy));
        // What the CALLER lacks, whatever the reason: the exact additions an operator
        // would make if the refusal was not intended. Authority errors point here.
        if let Some(me) = self.policy.actors.get(actor) {
            let no_caps: Vec<&str> = ALL_CAPABILITIES
                .iter()
                .copied()
                .filter(|c| !me.capabilities.iter().any(|h| h == c))
                .collect();
            let no_types: Vec<&str> = ALL_TYPES
                .iter()
                .copied()
                .filter(|t| !me.types.iter().any(|h| h == t))
                .collect();
            checks.push(if no_caps.is_empty() && no_types.is_empty() {
                check("authority.you", "ok", format!("actor {actor} holds every capability and built-in work type"), None)
            } else {
                let mut caps = me.capabilities.clone();
                caps.extend(no_caps.iter().map(|c| (*c).to_owned()));
                let mut types = me.types.clone();
                types.extend(no_types.iter().map(|t| (*t).to_owned()));
                check(
                    "authority.you",
                    "ok",
                    format!("actor {actor} does not hold capabilities [{}] or work types [{}]; operations needing them are refused (intended for a narrow actor)", no_caps.join(", "), no_types.join(", ")),
                    Some(format!("To grant everything, merge into config.json (keep the token) and restart:\n{}", serde_json::to_string_pretty(&json!({"actors":{actor:{"capabilities":caps,"types":types}}})).unwrap_or_default())),
                )
            });
        }
        Ok(report("service", checks))
    }

    /// Native workflows pending or enqueued that no admission accounts for (read-only).
    async fn orphan_workflows(&self) -> Result<Vec<String>> {
        let rows = self
            .native()?
            .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                names: vec![crate::core::WORKFLOW, crate::core::SCHEDULED],
                status: vec![
                    dbos::sysdb::types::WorkflowStatus::Pending,
                    dbos::sysdb::types::WorkflowStatus::Enqueued,
                ],
                load_input: false,
                load_output: false,
                ..Default::default()
            })
            .await?;
        let mut out = Vec::new();
        for row in rows {
            if !row.workflow_id.starts_with("sched-")
                && self.store.inspect(&row.workflow_id)?.is_none()
            {
                out.push(row.workflow_id);
            }
        }
        Ok(out)
    }
}

/// Human-readable rendering of a report.
pub fn render(report: &Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Forge doctor ({} checks): {}",
        report["mode"].as_str().unwrap_or("?"),
        report["status"].as_str().unwrap_or("?").to_uppercase()
    );
    for c in report["checks"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "[{:<4}] {:<20} {}",
            c["status"].as_str().unwrap_or("?"),
            c["id"].as_str().unwrap_or("?"),
            c["detail"].as_str().unwrap_or("")
        );
        if let Some(fix) = c["fix"].as_str() {
            for line in fix.lines() {
                let _ = writeln!(out, "         {line}");
            }
        }
    }
    let s = &report["summary"];
    let _ = writeln!(
        out,
        "\n{} ok, {} warn, {} fail, {} skipped. Doctor is read-only: it changed nothing.",
        s["ok"], s["warn"], s["fail"], s["skip"]
    );
    out
}

/// The CLI entry: ask the running service; if it cannot be reached, run the offline subset.
/// Exit status 1 when any check fails.
pub async fn cli(
    home: std::path::PathBuf,
    overrides: Overrides,
    url: String,
    token_env: &str,
    json: bool,
    offline_only: bool,
) -> Result<()> {
    let mut report = None;
    if !offline_only && let Ok(client) = crate::client::ApiClient::from_env(&url, token_env) {
        match client.invoke("doctor", json!({})).await {
            Ok(r) => report = Some(r),
            Err(e) if e.0.contains("could not be confirmed") => {}
            Err(e) => return Err(e.into()),
        }
    }
    let report = match report {
        Some(r) => r,
        None => offline(home, &overrides, &settings::process_env).await,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    if report["status"] == "fail" {
        return Err(crate::client::ExitStatus(1).into());
    }
    Ok(())
}

//! Human-readable operator CLI for workflows and runs. Every command calls the
//! canonical API (the same methods the dashboard and MCP use); `--json` prints
//! the documented stable machine result instead of text.
use crate::client::{ApiClient, ClientError};
use clap::Subcommand;
use serde_json::{Value, json};
use std::fmt::Write as _;

#[derive(Subcommand)]
pub enum Workflows {
    /// List owned workflow definitions.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one workflow's steps, dependencies, parameter defaults and output mapping.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Check a definition file exactly as registration would, without storing it.
    Validate {
        /// Workflow JSON: a full {"type":"workflow","input":{...}} definition or just its input
        /// ({"params":..,"steps":[..],"output":..}); '-' reads stdin.
        #[arg(long)]
        file: String,
        #[arg(long)]
        json: bool,
    },
    /// Register an immutable workflow definition under a new ID.
    Create {
        id: String,
        /// Same format as `validate --file`.
        #[arg(long)]
        file: String,
        #[arg(long)]
        json: bool,
    },
    /// Admit a run with parameters; prints the run ID and request key.
    Run {
        id: String,
        /// Parameter override KEY=VALUE; VALUE is parsed as JSON when valid, else text. Repeatable.
        #[arg(long = "param", value_name = "KEY=VALUE")]
        param: Vec<String>,
        /// Parameter object JSON file, or '-' for stdin; --param entries override it.
        #[arg(long)]
        params: Option<String>,
        /// Idempotency key. Reuse it after a lost response; omitted generates a new one.
        #[arg(long)]
        key: Option<String>,
        /// Wait for a terminal state and print step results; exits nonzero unless SUCCESS.
        #[arg(long)]
        wait: bool,
        /// Seconds to wait with --wait.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Allow new runs of a workflow.
    Enable { id: String },
    /// Stop new runs of a workflow (definitions are immutable and retained).
    Disable { id: String },
}

fn fail(message: impl Into<String>) -> anyhow::Error {
    ClientError(message.into()).into()
}

pub(crate) fn definition_file(path: &str) -> anyhow::Result<Value> {
    let value = crate::operations::json_input(path)?;
    // Accept the bare workflow input for convenience; the stored definition is always explicit.
    if value.get("type").is_none() && value.get("steps").is_some() {
        return Ok(json!({"type":"workflow","input":value}));
    }
    Ok(value)
}

pub(crate) fn parse_params(file: Option<&str>, pairs: &[String]) -> anyhow::Result<Option<Value>> {
    let mut params = match file {
        Some(path) => crate::operations::json_input(path)?,
        None if pairs.is_empty() => return Ok(None),
        None => json!({}),
    };
    let map = params
        .as_object_mut()
        .ok_or_else(|| fail("Parameters must be a JSON object"))?;
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .filter(|(k, _)| !k.is_empty())
            .ok_or_else(|| fail("--param requires KEY=VALUE"))?;
        let value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()));
        map.insert(key.to_owned(), value);
    }
    Ok(Some(params))
}

fn random_key() -> anyhow::Result<String> {
    let mut bytes = [0u8; 12];
    getrandom::getrandom(&mut bytes).map_err(|_| fail("random source unavailable"))?;
    Ok(format!(
        "cli-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

fn when(ms: &Value) -> String {
    ms.as_i64()
        .or_else(|| ms.as_f64().map(|f| (f * 1000.0) as i64))
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "-".into())
}

fn duration(step: &Value) -> String {
    match (step["started_at"].as_i64(), step["completed_at"].as_i64()) {
        (Some(a), Some(b)) if b >= a => format!("{:.2}s", (b - a) as f64 / 1000.0),
        _ => "-".into(),
    }
}

/// One-line, bounded rendering of a JSON value.
pub(crate) fn brief(value: &Value, width: usize) -> String {
    let text = match value {
        Value::Null => "-".to_owned(),
        Value::String(s) => s.clone(),
        Value::Object(o) if o.len() == 1 && o.contains_key("text") => o["text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| o["text"].to_string()),
        other => other.to_string(),
    };
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.chars().count() > width {
        format!(
            "{}…",
            flat.chars()
                .take(width.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        flat
    }
}

fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let line = |cells: Vec<&str>, out: &mut String| {
        let mut text = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                text.push_str(cell);
            } else {
                let pad = widths[i] - cell.chars().count();
                let _ = write!(text, "{cell}{}  ", " ".repeat(pad));
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    };
    line(headers.to_vec(), &mut out);
    for row in rows {
        line(row.iter().map(String::as_str).collect(), &mut out);
    }
    out
}

pub(crate) fn render_run(run: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Run       {}", run["run_id"].as_str().unwrap_or("-"));
    let _ = writeln!(
        out,
        "Work      {} ({})",
        run["work_id"].as_str().unwrap_or("-"),
        run["type"].as_str().unwrap_or("-")
    );
    let _ = writeln!(out, "Status    {}", run["status"].as_str().unwrap_or("-"));
    let _ = writeln!(out, "Created   {}", when(&run["created"]));
    if let Some(parent) = run["parent"].as_str() {
        let _ = writeln!(out, "Retry of  {parent}");
    }
    if run["params"].as_object().is_some_and(|p| !p.is_empty()) {
        let _ = writeln!(out, "Params    {}", brief(&run["params"], 200));
    }
    if let Some(d) = run["diagnostic"].as_str().filter(|d| !d.is_empty()) {
        let _ = writeln!(out, "Diagnosis {d}");
    }
    let failure = &run["workflow"]["failure"];
    if failure["location"] == "unknown" {
        let _ = writeln!(
            out,
            "Failure   {}",
            failure["reason"].as_str().unwrap_or("location unknown")
        );
        for d in failure["unverified_refusals"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let _ = writeln!(out, "Unverified refusal (not located) {}", brief(d, 200));
        }
    } else if !failure.is_null() {
        let _ = writeln!(
            out,
            "Failure   step {}: {}",
            failure["step"].as_str().unwrap_or("(workflow)"),
            failure["reason"].as_str().unwrap_or("-")
        );
        if failure["contradicted"] == true {
            let _ = writeln!(
                out,
                "          Other native evidence conflicts with this stop; inspect with soulsteads-forge runs show {} --json",
                run["run_id"].as_str().unwrap_or("<run>")
            );
        }
    }
    if let Some(steps) = run["workflow"]["steps"].as_array() {
        let rows: Vec<Vec<String>> = steps
            .iter()
            .map(|s| {
                vec![
                    s["index"].to_string(),
                    s["id"].as_str().unwrap_or("").into(),
                    s["state"].as_str().unwrap_or("").into(),
                    s["action"].as_str().unwrap_or("").into(),
                    duration(s),
                    if s["state"] == "failed" {
                        brief(&s["reason"], 70)
                    } else if s["output"].is_null() && s["note"].is_string() {
                        brief(&s["note"], 70)
                    } else {
                        brief(&s["output"], 70)
                    },
                ]
            })
            .collect();
        let _ = write!(
            out,
            "\nSteps\n{}",
            table(
                &["#", "STEP", "STATE", "ACTION", "TIME", "OUTPUT / REASON"],
                &rows
            )
        );
    } else if let Some(steps) = run["steps"].as_array().filter(|s| !s.is_empty()) {
        let rows: Vec<Vec<String>> = steps
            .iter()
            .map(|s| {
                vec![
                    s["function_id"].to_string(),
                    s["function_name"].as_str().unwrap_or("").into(),
                    if s["error"].is_null() {
                        "ok".into()
                    } else {
                        "error".into()
                    },
                    brief(&s["output"], 70),
                ]
            })
            .collect();
        let _ = write!(
            out,
            "\nNative steps\n{}",
            table(&["ID", "NAME", "RESULT", "OUTPUT"], &rows)
        );
    }
    if !run["output"].is_null() {
        let output = run["output"]
            .get("output")
            .filter(|_| run["type"] == "workflow")
            .unwrap_or(&run["output"]);
        let text = serde_json::to_string_pretty(output).unwrap_or_default();
        let shown: String = text.chars().take(4000).collect();
        let _ = writeln!(
            out,
            "\nOutput\n{shown}{}",
            if shown.len() < text.len() {
                "\n… (use --json or --step-id for complete values)"
            } else {
                ""
            }
        );
    }
    if let Some(effects) = run["effects"].as_array().filter(|e| !e.is_empty()) {
        let rows: Vec<Vec<String>> = effects
            .iter()
            .map(|e| {
                vec![
                    e["effect_id"].as_str().unwrap_or("").into(),
                    e["state"].as_str().unwrap_or("").into(),
                ]
            })
            .collect();
        let _ = write!(
            out,
            "\nExternal effects\n{}",
            table(&["EFFECT", "STATE"], &rows)
        );
    }
    let id = run["run_id"].as_str().unwrap_or("RUN_ID");
    let mut next = Vec::new();
    if run["actions"]["cancel"] == true {
        next.push(format!("soulsteads-forge runs cancel {id}"));
    }
    if run["actions"]["retry"] == true {
        next.push(format!("soulsteads-forge runs retry {id} --key NEW_KEY"));
    }
    if run["actions"]["reconcile"] == true {
        next.push(format!(
            "soulsteads-forge runs reconcile {id} --effect-id EFFECT --decision ... --note ... --settled"
        ));
    }
    if !next.is_empty() {
        let _ = writeln!(out, "\nAvailable: {}", next.join(" | "));
    }
    out
}

fn render_search(result: &Value) -> String {
    let rows: Vec<Vec<String>> = result["runs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|r| {
            vec![
                r["run_id"].as_str().unwrap_or("").into(),
                r["status"].as_str().unwrap_or("").into(),
                r["work_id"].as_str().unwrap_or("").into(),
                r["type"].as_str().unwrap_or("").into(),
                when(&r["created"]),
                if r["failure"].is_null() {
                    brief(&r["output"], 50)
                } else if r["failure"]["location"] == "unknown" {
                    "failed (location unknown)".into()
                } else {
                    format!(
                        "failed at {}",
                        r["failure"]["step"].as_str().unwrap_or("workflow")
                    )
                },
            ]
        })
        .collect();
    let mut out = if rows.is_empty() {
        "No runs match.\n".to_owned()
    } else {
        table(
            &["RUN", "STATUS", "WORK", "TYPE", "CREATED", "RESULT"],
            &rows,
        )
    };
    if let Some(cursor) = result["next_before"].as_str() {
        let _ = writeln!(out, "More: add --before {cursor}");
    }
    out
}

fn render_workflow(work: &Value) -> String {
    let d = &work["definition"];
    let mut out = String::new();
    let _ = writeln!(out, "Workflow  {}", work["id"].as_str().unwrap_or("-"));
    let _ = writeln!(
        out,
        "Enabled   {}",
        if work["enabled"] == true { "yes" } else { "no" }
    );
    let _ = writeln!(out, "Created   {}", when(&work["created"]));
    if d["type"] != "workflow" {
        let _ = writeln!(
            out,
            "Type      {} (not a workflow)",
            d["type"].as_str().unwrap_or("-")
        );
        return out;
    }
    let _ = writeln!(
        out,
        "Params    {}",
        brief(d["input"].get("params").unwrap_or(&json!({})), 200)
    );
    let rows: Vec<Vec<String>> = d["input"]["steps"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let action = &s["action"];
            vec![
                (i + 1).to_string(),
                s["id"].as_str().unwrap_or("").into(),
                s["needs"]
                    .as_array()
                    .map(|n| {
                        n.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "-".into()),
                action["work_id"]
                    .as_str()
                    .map(|w| format!("work:{w}"))
                    .unwrap_or_else(|| action["type"].as_str().unwrap_or("").into()),
                brief(
                    action
                        .get("input")
                        .or(action.get("params"))
                        .unwrap_or(&Value::Null),
                    60,
                ),
            ]
        })
        .collect();
    let _ = write!(
        out,
        "\nSteps\n{}",
        table(&["#", "STEP", "NEEDS", "ACTION", "INPUT"], &rows)
    );
    if let Some(output) = d["input"].get("output") {
        let _ = writeln!(out, "\nOutput mapping {}", brief(output, 200));
    } else {
        let _ = writeln!(out, "\nOutput: last step's output");
    }
    out
}

fn print(
    json_mode: bool,
    value: &Value,
    human: impl FnOnce(&Value) -> String,
) -> anyhow::Result<()> {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        print!("{}", human(value));
    }
    Ok(())
}

pub(crate) async fn run_search(
    client: &ApiClient,
    work: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
    before: Option<String>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let mut params = json!({});
    if let Some(work) = work {
        params["work_id"] = json!(work);
    }
    if let Some(status) = status {
        params["status"] = json!(status.to_ascii_uppercase());
    }
    if let Some(limit) = limit {
        params["limit"] = json!(limit);
    }
    if let Some(before) = before {
        params["before"] = json!(before);
    }
    let result = client.invoke("runs", params).await?;
    print(json_mode, &result, render_search)
}

pub(crate) async fn run_show(
    client: &ApiClient,
    id: String,
    json_mode: bool,
) -> anyhow::Result<()> {
    let run = client.invoke("inspect", json!({"run_id":id})).await?;
    print(json_mode, &run, render_run)
}

// ------------------------------------------------------------------ schedules
fn text_of(v: &Value) -> String {
    v.as_str().map(str::to_owned).unwrap_or_else(|| "-".into())
}

fn render_schedule_list(list: &Value) -> String {
    let items = list.as_array().map(Vec::as_slice).unwrap_or(&[]);
    if items.is_empty() {
        return "No schedules. Create one with: soulsteads-forge schedules create NAME --work-id ID --cron EXPR --tz ZONE\n".into();
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|s| {
            vec![
                text_of(&s["schedule_name"]),
                text_of(&s["status"]),
                text_of(&s["context"]["work_id"]),
                text_of(&s["schedule"]),
                text_of(&s["cron_timezone"]),
                text_of(&s["last_fired_at"]),
                s["next_run"]
                    .as_str()
                    .unwrap_or("- (not firing)")
                    .to_owned(),
                s["tags"]
                    .as_array()
                    .filter(|t| !t.is_empty())
                    .map(|t| {
                        t.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let mut out = table(
        &[
            "SCHEDULE",
            "STATUS",
            "WORK",
            "CRON",
            "ZONE",
            "LAST FIRED",
            "NEXT DUE",
            "TAGS",
        ],
        &rows,
    );
    out.push_str(
        "\nMissed ticks are skipped (no catch-up). Details: soulsteads-forge schedules show NAME\n",
    );
    out
}

fn occurrence_rows(occurrences: &[Value]) -> Vec<Vec<String>> {
    occurrences
        .iter()
        .map(|o| {
            let result = match o["outcome"].as_str() {
                Some("failed") => {
                    let f = &o["failure"];
                    if f.is_object() {
                        format!(
                            "failed at {}: {}",
                            f["step"].as_str().unwrap_or("workflow"),
                            brief(&f["reason"], 60)
                        )
                    } else {
                        brief(
                            if o["run"]["error"].is_null() {
                                &o["run"]["diagnostic"]
                            } else {
                                &o["run"]["error"]
                            },
                            70,
                        )
                    }
                }
                Some("not_admitted") => brief(&o["handoff_error"], 70),
                // A workflow publishes its mapped `output`; other work's output is the result.
                _ => brief(
                    o["run"]["output"]
                        .get("output")
                        .unwrap_or(&o["run"]["output"]),
                    70,
                ),
            };
            vec![
                text_of(&o["scheduled_time"]),
                if o["kind"] == "manual" {
                    "manual run".into()
                } else {
                    "due tick".into()
                },
                text_of(&o["handoff"]),
                text_of(&o["outcome"]),
                text_of(&o["run_id"]),
                result,
            ]
        })
        .collect()
}

fn render_trigger(t: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Manual run of {} (not a due tick); schedule stays {}.",
        text_of(&t["schedule_name"]),
        text_of(&t["schedule_status"])
    );
    let _ = writeln!(out, "Trigger  {}", text_of(&t["trigger_id"]));
    let _ = writeln!(
        out,
        "Work     {}  params {}",
        text_of(&t["work_id"]),
        brief(&t["params"], 80)
    );
    let _ = writeln!(
        out,
        "Handoff  {}  outcome {}",
        text_of(&t["handoff"]),
        text_of(&t["outcome"])
    );
    match t["run_id"].as_str() {
        Some(run) => {
            let _ = writeln!(out, "Run      {run}");
            let _ = writeln!(out, "{}", text_of(&t["note"]));
            let _ = writeln!(out, "Result: soulsteads-forge runs show {run}");
        }
        None => {
            let _ = writeln!(out, "Run      not admitted yet");
            let _ = writeln!(out, "{}", text_of(&t["note"]));
        }
    }
    let _ = writeln!(
        out,
        "Each `schedules run` starts another new run; it is never retried automatically."
    );
    out
}

fn render_history(value: &Value) -> String {
    let items = value["occurrences"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if items.is_empty() {
        return "No due occurrences yet.\n".into();
    }
    let mut out = table(
        &["TIME", "KIND", "HANDOFF", "OUTCOME", "RUN", "RESULT"],
        &occurrence_rows(items),
    );
    out.push_str("\nHANDOFF is the native DBOS firing that admits the run; OUTCOME is the admitted run's own result.\nInspect a run: soulsteads-forge runs show RUN_ID\n");
    out
}

fn render_schedule(s: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Schedule {}  [{}]",
        text_of(&s["schedule_name"]),
        text_of(&s["status"])
    );
    let _ = writeln!(out, "Work:        {}", text_of(&s["context"]["work_id"]));
    let _ = writeln!(
        out,
        "Cron:        {}  (zone {})",
        text_of(&s["schedule"]),
        text_of(&s["cron_timezone"])
    );
    if !s["params"].is_null() {
        let _ = writeln!(out, "Params:      {}", brief(&s["params"], 100));
    }
    if let Some(tags) = s["tags"].as_array().filter(|t| !t.is_empty()) {
        let _ = writeln!(
            out,
            "Tags:        {}",
            tags.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if s["revision"].as_i64().unwrap_or(0) > 0 {
        let _ = writeln!(out, "Revision:    {} (edited in place)", s["revision"]);
    }
    let _ = writeln!(out, "Catch-up:    {}", text_of(&s["catchup"]));
    let _ = writeln!(out, "Last fired:  {}", text_of(&s["last_fired_at"]));
    let upcoming = s["upcoming"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if upcoming.is_empty() {
        let _ = writeln!(
            out,
            "Next due:    - (not firing while {})",
            text_of(&s["status"])
        );
    } else {
        for (i, t) in upcoming.iter().enumerate() {
            let _ = writeln!(
                out,
                "{}{}  ({} UTC)",
                if i == 0 {
                    "Next due:    "
                } else {
                    "             "
                },
                text_of(&t["local"]),
                text_of(&t["utc"])
            );
        }
    }
    if let Some(counts) = s["outcome_counts"].as_object().filter(|c| !c.is_empty()) {
        let parts: Vec<String> = counts.iter().map(|(k, v)| format!("{k} {v}")).collect();
        let _ = writeln!(
            out,
            "Outcomes:    {} (last {} occurrences)",
            parts.join(" · "),
            s["limit"]
        );
    }
    let a = &s["actions"];
    let mut next = Vec::new();
    let name = text_of(&s["schedule_name"]);
    if a["pause"] == true {
        next.push(format!("soulsteads-forge schedules pause {name}"));
    }
    if a["resume"] == true {
        next.push(format!("soulsteads-forge schedules resume {name}"));
    }
    if a["delete"] == true {
        next.push(format!("soulsteads-forge schedules delete {name}"));
    }
    if a["edit"] == true {
        next.push(format!(
            "soulsteads-forge schedules edit {name} --cron EXPR --tz ZONE [--tag T]"
        ));
    }
    let _ = writeln!(out, "\nRecent occurrences (newest first):");
    let items = s["occurrences"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if items.is_empty() {
        out.push_str("  none yet\n");
    } else {
        out.push_str(&table(
            &["TIME", "KIND", "HANDOFF", "OUTCOME", "RUN", "RESULT"],
            &occurrence_rows(items),
        ));
    }
    let _ = writeln!(out, "\nNext actions:");
    for n in next {
        let _ = writeln!(out, "  {n}");
    }
    let _ = writeln!(out, "  soulsteads-forge runs show RUN_ID");
    let _ = writeln!(out, "Editing: {}", text_of(&s["replacement"]));
    out
}

fn render_preview(p: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Valid: {} in {}",
        text_of(&p["schedule"]),
        text_of(&p["cron_timezone"])
    );
    for t in p["upcoming"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "  {}  ({} UTC)",
            text_of(&t["local"]),
            text_of(&t["utc"])
        );
    }
    let _ = writeln!(out, "Catch-up: {}", text_of(&p["catchup"]));
    let _ = writeln!(out, "{}", text_of(&p["authority"]));
    out
}

/// One stderr line confirming a schedule mutation; stdout keeps the stable JSON.
pub(crate) fn schedule_mutation_note(
    method: &str,
    params: &Value,
    result: &Value,
) -> Option<String> {
    let find = |name: &str| {
        result
            .as_array()
            .and_then(|l| l.iter().find(|s| s["schedule_name"] == name))
            .cloned()
    };
    match method {
        "schedule" => {
            // The created identity is exactly `<actor>.<submitted local name>`; a suffix
            // match would pick e.g. an existing `operator.a.digest` for a new `digest`.
            let local = params["name"].as_str()?;
            let s = result.as_array()?.iter().find(|s| {
                match (s["schedule_name"].as_str(), s["context"]["actor"].as_str()) {
                    (Some(n), Some(actor)) => n == format!("{actor}.{local}"),
                    _ => false,
                }
            })?;
            Some(format!(
                "Created {} [{}]: {} in {}; next due {}. Missed ticks are skipped. Watch: soulsteads-forge schedules show {}",
                text_of(&s["schedule_name"]),
                text_of(&s["status"]),
                text_of(&s["schedule"]),
                text_of(&s["cron_timezone"]),
                text_of(&s["next_run"]),
                text_of(&s["schedule_name"])
            ))
        }
        "schedule_update" => {
            let changed: Vec<&str> = result["changed"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            Some(if changed.is_empty() {
                format!("{}: nothing changed.", text_of(&result["schedule_name"]))
            } else {
                format!(
                    "Updated {} ({}); next due {}. History kept; no tick fires twice.",
                    text_of(&result["schedule_name"]),
                    changed.join(", "),
                    result["upcoming"][0]["local"].as_str().unwrap_or("-")
                )
            })
        }
        "schedule_action" => {
            let name = params["name"].as_str()?;
            match params["action"].as_str()? {
                "pause" => Some(format!(
                    "Paused {name}; no further ticks are admitted until resumed, and ticks missed meanwhile are skipped."
                )),
                "resume" => Some(format!(
                    "Resumed {name}; next due {}.",
                    find(name)
                        .map(|s| text_of(&s["next_run"]))
                        .unwrap_or_else(|| "-".into())
                )),
                "delete" => Some(format!(
                    "Deleted {name}; already admitted runs continue and its history stays readable with: soulsteads-forge schedules history {name}"
                )),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) async fn schedules(
    client: &ApiClient,
    command: crate::operations::Schedules,
) -> anyhow::Result<()> {
    use crate::operations::Schedules as S;
    match command {
        S::Status { json, tag } => {
            let list = match tag {
                Some(tag) => client.invoke("schedules", json!({"tag":tag})).await?,
                None => client.invoke("snapshot", json!({})).await?["schedules"].clone(),
            };
            print(json, &list, render_schedule_list)
        }
        S::Show { name, limit, json } => {
            let mut params = json!({"name":name});
            if let Some(limit) = limit {
                params["limit"] = json!(limit);
            }
            let value = client.invoke("schedule_show", params).await?;
            print(json, &value, render_schedule)
        }
        S::History { name, limit, json } => {
            let mut params = json!({"name":name});
            if let Some(limit) = limit {
                params["limit"] = json!(limit);
            }
            let value = client.invoke("schedule_occurrences", params).await?;
            print(json, &value, render_history)
        }
        S::Check {
            cron,
            tz,
            count,
            json,
        } => {
            let mut params = json!({"cron":cron,"tz":tz});
            if let Some(count) = count {
                params["count"] = json!(count);
            }
            let value = client.invoke("schedule_preview", params).await?;
            print(json, &value, render_preview)
        }
        S::Run { name, json } => {
            let value = client
                .invoke("schedule_trigger", json!({"name": name}))
                .await
                .map_err(|e| {
                    fail(format!(
                        "{e}\nNothing was retried. Running again always starts another run; first check: soulsteads-forge schedules history {name}"
                    ))
                })?;
            print(json, &value, render_trigger)
        }
        _ => Err(fail("not a human schedule view")),
    }
}

// ------------------------------------------------------------------ queues
fn seconds(ms: &Value) -> String {
    ms.as_i64()
        .map(|m| format!("{:.1}s", m as f64 / 1000.0))
        .unwrap_or_else(|| "-".into())
}

fn short_id(v: &Value) -> String {
    let id = text_of(v);
    if id.chars().count() > 20 {
        format!("{}…", id.chars().take(19).collect::<String>())
    } else {
        id
    }
}

fn render_policy(q: &Value) -> String {
    let p = &q["policy"];
    if p.is_null() {
        return "no stored native policy (DBOS built-in)".into();
    }
    let mut parts = vec![match p["concurrency"].as_i64() {
        Some(c) => format!("concurrency {c}"),
        None => "no global concurrency limit".into(),
    }];
    if let Some(w) = p["worker_concurrency"].as_i64() {
        parts.push(format!("per-executor {w}"));
    }
    if p["partitioned"] == true {
        parts.push(format!(
            "partitioned: {} per partition",
            p["partition_concurrency"]
                .as_i64()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unlimited".into())
        ));
    }
    parts.push(if p["rate_limit"].is_null() {
        "no rate limit".into()
    } else {
        format!(
            "rate limit {} per {} ms",
            p["rate_limit"]["limit"], p["rate_limit"]["period_ms"]
        )
    });
    parts.push(format!("polls every {} ms", p["polling_interval_ms"]));
    parts.join("; ")
}

fn render_queue(q: &Value, detail: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Queue {}  ({})",
        text_of(&q["key"]),
        text_of(&q["name"])
    );
    let _ = writeln!(out, "  {}", text_of(&q["summary"]));
    let _ = writeln!(out, "  Native policy: {}", render_policy(q));
    if detail || q["key"] != "work" {
        let _ = writeln!(out, "  {}", text_of(&q["role"]));
    }
    let members = q["members"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let running: Vec<&Value> = members.iter().filter(|m| m["state"] == "running").collect();
    let waiting: Vec<&Value> = members.iter().filter(|m| m["state"] != "running").collect();
    let id = |m: &Value| {
        if m["kind"] == "run" {
            short_id(&m["run_id"])
        } else {
            format!("tick of {}", text_of(&m["schedule_name"]))
        }
    };
    if !running.is_empty() {
        let _ = writeln!(out, "\n  Running (yours):");
        let rows: Vec<Vec<String>> = running
            .iter()
            .map(|m| {
                let a = &m["activity"];
                let doing = match a["kind"].as_str() {
                    Some("durable_sleep") => format!("durable sleep until {}", when(&a["until"])),
                    Some("executing") => format!(
                        "executing; last checkpoint {} ({})",
                        a["last_checkpoint"]["step_id"],
                        text_of(&a["last_checkpoint"]["name"])
                    ),
                    Some("started") => "dequeued; no checkpoint yet".into(),
                    _ => "schedule handoff".into(),
                };
                vec![id(m), text_of(&m["work_id"]), when(&m["started_at"]), doing]
            })
            .collect();
        for line in table(&["RUN", "WORK", "STARTED", "DOING"], &rows).lines() {
            let _ = writeln!(out, "    {line}");
        }
    }
    if !waiting.is_empty() {
        let _ = writeln!(out, "\n  Waiting (yours):");
        let rows: Vec<Vec<String>> = waiting
            .iter()
            .map(|m| {
                vec![
                    id(m),
                    text_of(&m["work_id"]),
                    brief(&m["params"], 40),
                    seconds(&m["wait"]["waiting_ms"]),
                    text_of(&m["wait"]["reason"]),
                ]
            })
            .collect();
        for line in table(&["RUN", "WORK", "PARAMS", "WAITING", "REASON"], &rows).lines() {
            let _ = writeln!(out, "    {line}");
        }
        let mut seen = std::collections::BTreeSet::new();
        for m in &waiting {
            let e = text_of(&m["wait"]["explanation"]);
            if seen.insert(e.clone()) {
                let _ = writeln!(out, "    Why: {e}");
            }
        }
        let _ = writeln!(
            out,
            "    Position: not reported (DBOS exposes no queue position or start estimate)."
        );
    }
    let listing = &q["members_listing"];
    if listing["identities_listed"] == true {
        if listing["complete"] == true {
            if members.is_empty() {
                let _ = writeln!(out, "  None of your work is running or waiting here.");
            }
        } else {
            let _ = writeln!(
                out,
                "  Your listed work is INCOMPLETE ({} of {} found shown; native scan {}): {}",
                listing["shown"],
                listing["found_in_scan"],
                if listing["native_scan_complete"] == true {
                    "complete"
                } else {
                    "incomplete"
                },
                text_of(&listing["note"])
            );
        }
    }
    let recent = q["recent"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let window = &q["recent_window"];
    if detail && recent.is_empty() && !window.is_null() {
        let _ = writeln!(
            out,
            "\n  Recent results (yours): {}",
            if window["complete"] == true {
                "none recorded on this queue.".to_owned()
            } else if window["unavailable"].as_u64().unwrap_or(0) > 0 {
                format!("none shown. {}", text_of(&window["note"]))
            } else {
                format!("none found in the sample. {}", text_of(&window["note"]))
            }
        );
    }
    if detail && !recent.is_empty() {
        let _ = writeln!(
            out,
            "\n  Recent results (yours, newest first{}):",
            if window["complete"] == true {
                ""
            } else {
                "; SAMPLED"
            }
        );
        let rows: Vec<Vec<String>> = recent
            .iter()
            .map(|r| {
                let output = r["output"].get("output").unwrap_or(&r["output"]);
                vec![
                    short_id(&r["run_id"]),
                    text_of(&r["work_id"]),
                    text_of(&r["status"]),
                    seconds(&r["waited_ms"]),
                    seconds(&r["ran_ms"]),
                    brief(output, 60),
                ]
            })
            .collect();
        for line in table(&["RUN", "WORK", "STATUS", "WAITED", "RAN", "RESULT"], &rows).lines() {
            let _ = writeln!(out, "    {line}");
        }
        if window["complete"] != true {
            let _ = writeln!(out, "    {}", text_of(&window["note"]));
        }
    }
    out
}

fn render_queues(v: &Value) -> String {
    let mut out = String::new();
    let queues = v["queues"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let detail = queues.len() == 1;
    for q in queues {
        out.push_str(&render_queue(q, detail));
        out.push('\n');
    }
    let pending = v["admitted_not_on_queue"]["yours"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !pending.is_empty() {
        let _ = writeln!(out, "Admitted, not yet on the native queue (yours):");
        for p in pending {
            let _ = writeln!(
                out,
                "  {} {} [{}]",
                text_of(&p["run_id"]),
                text_of(&p["work_id"]),
                text_of(&p["admission_state"])
            );
        }
        if let Some(e) = pending.first().map(|p| text_of(&p["explanation"])) {
            let _ = writeln!(out, "  {e}");
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "Counts are home-local and include work outside your scope (counted, not identified)."
    );
    let has_members = queues
        .iter()
        .any(|q| q["members_shown"].as_i64().unwrap_or(0) > 0);
    if has_members {
        let _ = writeln!(
            out,
            "Next: soulsteads-forge runs show RUN_ID  ·  soulsteads-forge runs cancel RUN_ID (a waiting run cancelled before dequeue never starts)"
        );
    }
    if !detail {
        let _ = writeln!(
            out,
            "Details and recent results: soulsteads-forge queues show work"
        );
    }
    out
}

pub(crate) async fn queues(
    client: &ApiClient,
    command: crate::operations::Queues,
) -> anyhow::Result<()> {
    use crate::operations::Queues as Q;
    match command {
        Q::Status { json } => {
            let value = client.invoke("queues", json!({})).await?;
            print(json, &value, render_queues)
        }
        Q::Show { queue, limit, json } => {
            let mut params = json!({"queue": queue});
            if let Some(limit) = limit {
                params["limit"] = json!(limit);
            }
            let value = client.invoke("queues", params).await?;
            print(json, &value, render_queues)
        }
    }
}

const ACTIVE: &[&str] = &["ADMITTED", "PENDING", "ENQUEUED", "DELAYED"];

pub(crate) async fn workflows(client: &ApiClient, command: Workflows) -> anyhow::Result<()> {
    match command {
        Workflows::List { json } => {
            let snapshot = client.invoke("snapshot", json!({})).await?;
            let list: Vec<Value> = snapshot["work"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .filter(|w| w["definition"]["type"] == "workflow")
                .map(|w| json!({"id":w["id"],"enabled":w["enabled"],"created":w["created"]}))
                .collect();
            let value = json!({"workflows":list});
            print(json, &value, |v| {
                let rows: Vec<Vec<String>> = v["workflows"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(|w| {
                        vec![
                            w["id"].as_str().unwrap_or("").into(),
                            if w["enabled"] == true {
                                "yes".into()
                            } else {
                                "no".into()
                            },
                            when(&w["created"]),
                        ]
                    })
                    .collect();
                if rows.is_empty() {
                    "No workflows. Create one with: soulsteads-forge workflows create ID --file workflow.json\n".into()
                } else {
                    table(&["WORKFLOW", "ENABLED", "CREATED"], &rows)
                }
            })
        }
        Workflows::Show { id, json } => {
            let work = client.invoke("work", json!({"work_id":id})).await?;
            print(json, &work, render_workflow)
        }
        Workflows::Validate { file, json } => {
            let definition = definition_file(&file)?;
            let result = client
                .invoke("validate", json!({"definition":definition}))
                .await?;
            print(json, &result, |r| {
                let steps = r["steps"].as_array().map(Vec::len).unwrap_or(0);
                format!(
                    "Valid {} definition with {steps} step(s).\n",
                    r["type"].as_str().unwrap_or("-")
                )
            })
        }
        Workflows::Create { id, file, json } => {
            let definition = definition_file(&file)?;
            let work = client
                .invoke("register", json!({"work_id":id,"definition":definition}))
                .await?;
            print(json, &work, |w| {
                format!(
                    "Registered workflow {}.\n{}",
                    w["id"].as_str().unwrap_or("-"),
                    render_workflow(w)
                )
            })
        }
        Workflows::Enable { id } => {
            client
                .invoke("enable", json!({"work_id":id,"enabled":true}))
                .await?;
            println!("Workflow {id} enabled.");
            Ok(())
        }
        Workflows::Disable { id } => {
            client
                .invoke("enable", json!({"work_id":id,"enabled":false}))
                .await?;
            println!("Workflow {id} disabled; existing runs and history are retained.");
            Ok(())
        }
        Workflows::Run {
            id,
            param,
            params,
            key,
            wait,
            timeout,
            json,
        } => {
            let key = match key {
                Some(key) => key,
                None => random_key()?,
            };
            let mut request = json!({"work_id":id,"key":key});
            if let Some(params) = parse_params(params.as_deref(), &param)? {
                request["params"] = params;
            }
            let admitted = client.invoke("run", request).await?;
            let run_id = admitted["run_id"]
                .as_str()
                .ok_or_else(|| fail("Forge returned no run ID"))?
                .to_owned();
            let mut result = json!({"run_id":run_id,"key":key,"work_id":id});
            if !wait {
                return print(json, &result, |r| {
                    format!(
                        "Admitted run {}\nRequest key {} (reuse with --key to safely repeat after a lost response)\nFollow with: soulsteads-forge runs show {}\n",
                        r["run_id"].as_str().unwrap_or(""),
                        r["key"].as_str().unwrap_or(""),
                        r["run_id"].as_str().unwrap_or("")
                    )
                });
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            let run = loop {
                let run = client.invoke("inspect", json!({"run_id":run_id})).await?;
                let status = run["status"].as_str().unwrap_or("ADMITTED");
                if !ACTIVE.contains(&status) || std::time::Instant::now() >= deadline {
                    break run;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            };
            result["run"] = run.clone();
            if !json {
                println!("Request key {key}");
            }
            print(json, &result, |r| render_run(&r["run"]))?;
            let status = run["status"].as_str().unwrap_or("");
            if ACTIVE.contains(&status) {
                return Err(fail(format!(
                    "Run {run_id} is still {status} after {timeout}s; follow with soulsteads-forge runs show {run_id}"
                )));
            }
            if status != "SUCCESS" {
                return Err(fail(format!("Run {run_id} finished {status}")));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn params_merge_typed_values_and_reject_malformed_pairs() {
        let p = parse_params(
            None,
            &["n=3".into(), "s=hello".into(), "o={\"a\":1}".into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(p, json!({"n":3,"s":"hello","o":{"a":1}}));
        assert!(parse_params(None, &["novalue".into()]).is_err());
        assert!(parse_params(None, &[]).unwrap().is_none());
    }
    #[test]
    fn run_rendering_names_failed_step_and_next_actions() {
        let run = json!({"run_id":"r1","work_id":"w","type":"workflow","status":"ERROR","created":1_700_000_000_000i64,
            "workflow":{"failure":{"step":"b","reason":"root/step:b: explicit flow failure"},"steps":[
                {"index":1,"id":"a","state":"completed","action":"echo","output":{"text":"hi"},"started_at":1,"completed_at":501},
                {"index":2,"id":"b","state":"failed","action":"fail","output":null,"reason":"root/step:b: explicit flow failure"}]},
            "output":null,"actions":{"cancel":false,"retry":true,"reconcile":false}});
        let text = render_run(&run);
        assert!(text.contains("Failure   step b: root/step:b: explicit flow failure"));
        assert!(text.contains("hi"));
        assert!(text.contains("0.50s"));
        assert!(text.contains("soulsteads-forge runs retry r1 --key NEW_KEY"));
        assert!(!text.contains("runs cancel"));
    }
    #[test]
    fn run_rendering_reports_unlocatable_failure_without_naming_a_step() {
        let def = json!({"type":"workflow","input":{"steps":[
            {"id":"group","action":{"type":"workflow","input":{"steps":[{"id":"inner","action":{"type":"echo","input":{"text":"ok"}}}]}}},
            {"id":"use","action":{"type":"echo","input":{"text":"${/params/missing}"}}},
            {"id":"after","action":{"type":"echo","input":{"text":"z"}}}]}});
        let mut run = json!({"run_id":"r9","work_id":"w","type":"workflow","status":"ERROR","created":1_700_000_000_000i64,
            "output":null,"actions":{"cancel":false,"retry":true,"reconcile":false},
            "steps":[{"function_id":1,"function_name":"forge.flow.root/step:after","output":{"text":"z"},"error":null}],
            "logs":[{"action":"flow_refused","detail":"root/step:use: input reference failed: x"}]});
        run["workflow"] = crate::run_views::workflow_steps(&def, &run).unwrap();
        let text = render_run(&run);
        assert!(text.contains("Failure   Location unknown: "));
        assert!(text.contains("soulsteads-forge runs show r9 --json"));
        assert!(
            text.contains(
                "Unverified refusal (not located) root/step:use: input reference failed: x"
            )
        );
        assert!(!text.contains("Failure   step"));
        assert!(!text.contains("not_reached"));
        assert!(text.contains("unknown"));
        let list = render_search(
            &json!({"runs":[{"run_id":"r9","status":"ERROR","work_id":"w","type":"workflow","created":1,"failure":run["workflow"]["failure"]}],"next_before":null,"scanned":1}),
        );
        assert!(list.contains("failed (location unknown)"));
    }

    #[test]
    fn manual_run_rendering_names_trigger_and_is_distinct_from_a_due_tick() {
        let t = json!({"schedule_name":"operator.digest","kind":"manual","trigger_id":"sched-operator.digest-trigger-2026-11-16T21:00:00.1Z","work_id":"digest","params":{"team":"x"},"handoff":"SUCCESS","run_id":"forge-v1-abc","outcome":"running","schedule_status":"PAUSED","note":"n"});
        let text = render_trigger(&t);
        assert!(
            text.starts_with(
                "Manual run of operator.digest (not a due tick); schedule stays PAUSED."
            )
        );
        assert!(
            text.contains("Trigger  sched-operator.digest-trigger-")
                && text.contains("soulsteads-forge runs show forge-v1-abc")
        );
        assert!(text.contains("never retried automatically"));
        let pending =
            json!({"schedule_name":"s","trigger_id":"t","run_id":null,"note":"Do not run again"});
        assert!(render_trigger(&pending).contains("Run      not admitted yet"));
        let rows = occurrence_rows(&[
            json!({"kind":"manual","outcome":"succeeded"}),
            json!({"kind":"due","outcome":"succeeded"}),
        ]);
        assert_eq!(
            (rows[0][1].as_str(), rows[1][1].as_str()),
            ("manual run", "due tick")
        );
    }
    #[test]
    fn queue_view_renders_uninspectable_owned_result_and_omitted_delayed_as_not_absent() {
        // SYNTHETIC: complete scan, running shown at limit 1, delayed omitted; one owned
        // finished run whose inspection failed in a short window.
        let v = json!({"queues":[{"key":"work","name":"w","summary":"s","policy":null,"role":"r",
            "members":[{"kind":"run","state":"running","run_id":"forge-v1-aaaaaaaaaaaaaaaaaaaaaaaa","work_id":"held","started_at":0,"activity":{"kind":"started"}}],
            "members_shown":1,"members_yours":2,
            "members_listing":{"identities_listed":true,"complete":false,"native_scan_complete":true,"shown":1,"found_in_scan":2,"note":"1 more of your workflows were found but not listed (member limit 1)."},
            "recent":[],"recent_window":{"complete":false,"unavailable":1,"note":"1 of your finished runs in this window could not be read for display right now; they are recorded, not absent. Open them from Runs. See Runs (soulsteads-forge runs search) for full history."}}],
            "admitted_not_on_queue":{"yours":[],"home_total":0}});
        let text = render_queues(&v);
        assert!(
            text.contains(
                "Your listed work is INCOMPLETE (1 of 2 found shown; native scan complete)"
            )
        );
        assert!(!text.contains("None of your work") && !text.contains("none recorded"));
        assert!(text.contains("Recent results (yours): none shown. 1 of your finished runs in this window could not be read"));
    }
    #[test]
    fn queue_view_renders_incomplete_members_and_sampled_recent_as_unknown() {
        // Synthetic boundary: older partner rows filled the scan; recent window full of partner results.
        let v = json!({"queues":[{"key":"work","name":"w","summary":"at least 1 of 2 slots running; 499 waiting; 0 delayed. Free slots unknown.",
            "policy":null,"role":"r","members":[],"members_shown":0,"members_yours":0,
            "members_listing":{"identities_listed":true,"complete":false,"native_scan_complete":false,"shown":0,"found_in_scan":0,"note":"The native scan read only the oldest 500 nonterminal workflows on this queue, so work of yours created later is not visible here."},
            "recent":[],"recent_window":{"complete":false,"note":"Sampled: only the newest 100 finished workflows on this queue (all actors) are examined."}}],
            "admitted_not_on_queue":{"yours":[],"home_total":0}});
        let text = render_queues(&v);
        assert!(text.contains(
            "Your listed work is INCOMPLETE (0 of 0 found shown; native scan incomplete)"
        ));
        assert!(!text.contains("None of your work"));
        assert!(text.contains("none found in the sample. Sampled: only the newest 100"));
        assert!(!text.contains("none recorded"));
    }
    #[test]
    fn queue_view_explains_waiting_without_a_position_and_lists_results() {
        let v = json!({"queues":[{"key":"work","name":"forge-v1-h-work","summary":"2 of 2 slots running; 1 waiting; 0 delayed (1 outside your scope, counted only).",
            "policy":{"concurrency":2,"worker_concurrency":null,"rate_limit":null,"partitioned":false,"polling_interval_ms":1000},
            "role":"r","members_shown":2,"members_yours":2,
            "members":[
              {"kind":"run","state":"running","run_id":"forge-v1-aaaaaaaaaaaaaaaaaaaaaaaa","work_id":"held","started_at":0,"activity":{"kind":"durable_sleep","until":60000}},
              {"kind":"run","state":"waiting","run_id":"forge-v1-bbbbbbbbbbbbbbbbbbbbbbbb","work_id":"held","params":{"item":"beta"},"wait":{"reason":"queue_capacity","explanation":"Every work slot is occupied","waiting_ms":1500,"position":null}}],
            "members_listing":{"identities_listed":true,"complete":true,"native_scan_complete":true,"shown":2,"found_in_scan":2},
            "recent_window":{"complete":true,"note":"x"},
            "recent":[{"run_id":"forge-v1-cccccccccccccccccccccccc","work_id":"held","status":"SUCCESS","waited_ms":45000,"ran_ms":45100,"output":{"output":{"report":"Report for gamma: ready"}}}]}],
            "admitted_not_on_queue":{"yours":[],"home_total":0}});
        let text = render_queues(&v);
        assert!(text.contains("durable sleep until 1970-01-01 00:01:00 UTC"));
        assert!(
            text.contains("queue_capacity") && text.contains("Why: Every work slot is occupied")
        );
        assert!(text.contains("Position: not reported"));
        assert!(text.contains("45.0s") && text.contains("Report for gamma: ready"));
        assert!(text.contains("concurrency 2; no rate limit"));
    }
    #[test]
    fn schedule_views_separate_handoff_from_run_outcome_and_name_actions() {
        use crate::core::occurrence_outcome;
        assert_eq!(occurrence_outcome("SUCCESS", Some("SUCCESS")), "succeeded");
        assert_eq!(occurrence_outcome("SUCCESS", Some("ERROR")), "failed");
        assert_eq!(occurrence_outcome("ERROR", Some("SUCCESS")), "succeeded");
        assert_eq!(occurrence_outcome("ERROR", None), "not_admitted");
        assert_eq!(occurrence_outcome("ENQUEUED", None), "handoff_pending");
        assert_eq!(occurrence_outcome("SUCCESS", Some("PENDING")), "running");
        let show = json!({"schedule_name":"operator.digest","status":"PAUSED","schedule":"0 */1 * * * *","cron_timezone":"America/Chicago","context":{"work_id":"digest"},"params":{"topic":"x"},"catchup":crate::core::CATCHUP,"last_fired_at":"2026-11-16T20:01:00+00:00","upcoming":[],"outcome_counts":{"succeeded":1,"failed":1},"limit":20,"actions":{"pause":false,"resume":true,"delete":true,"replace":true},"replacement":crate::core::REPLACEMENT,
            "occurrences":[
                {"scheduled_time":"2026-11-16T20:01:00+00:00","handoff":"SUCCESS","outcome":"failed","run_id":"r2","failure":{"step":"publish","reason":"boom"},"run":{"status":"ERROR"}},
                {"scheduled_time":"2026-11-16T20:00:00+00:00","handoff":"SUCCESS","outcome":"succeeded","run_id":"r1","failure":null,"run":{"status":"SUCCESS","output":{"text":"done"}}}]});
        let text = render_schedule(&show);
        assert!(text.contains("[PAUSED]") && text.contains("America/Chicago"));
        assert!(text.contains("not firing while PAUSED"));
        assert!(text.contains("failed at publish: boom") && text.contains("done"));
        assert!(
            text.contains("schedules resume operator.digest")
                && !text.contains("schedules pause operator.digest")
        );
        assert!(text.contains("replacement under a new name"));
        let note = schedule_mutation_note(
            "schedule_action",
            &json!({"name":"operator.digest","action":"pause"}),
            &json!([]),
        )
        .unwrap();
        assert!(note.contains("Paused operator.digest"));
        assert!(render_schedule_list(&json!([])).contains("No schedules"));
    }

    #[test]
    fn create_note_names_the_exact_created_schedule_not_a_suffix_match() {
        let row = |name: &str, actor: &str| json!({"schedule_name":name,"status":"ACTIVE","schedule":"0 0 9 * * *","cron_timezone":"UTC","next_run":"n","context":{"actor":actor,"work_id":"digest","params":{}}});
        let created = |local: &str, list: Value| {
            schedule_mutation_note("schedule", &json!({"name":local}), &list)
                .map(|n| n.split_whitespace().nth(1).unwrap().to_string())
        };
        // DBOS orders by name, so the older operator.a.digest precedes operator.digest.
        let list = json!([
            row("operator.a.digest", "operator"),
            row("operator.digest", "operator")
        ]);
        assert_eq!(
            created("digest", list.clone()).as_deref(),
            Some("operator.digest")
        );
        assert_eq!(
            created("a.digest", list).as_deref(),
            Some("operator.a.digest")
        );
        // Dotted actor: never split the canonical name on dots.
        let list = json!([
            row("ops.team.a.digest", "ops.team"),
            row("ops.team.digest", "ops.team")
        ]);
        assert_eq!(created("digest", list).as_deref(), Some("ops.team.digest"));
        // No exact row: say nothing rather than guess.
        let list = json!([row("operator.a.digest", "operator")]);
        assert_eq!(created("digest", list), None);
    }
}

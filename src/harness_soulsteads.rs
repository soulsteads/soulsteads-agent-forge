//! Narrow public CLI contract for soulsteads-harness (source baseline 549aa21).
use super::*;

/// No inherited native config, arbitrary args, MCP children, or config-loading env.
pub(super) fn validate(profile: &RunnerProfile) -> Result<()> {
    if !profile.args.is_empty()
        || profile
            .model
            .as_deref()
            .is_none_or(|m| m.trim().is_empty() || m.trim() != m)
        || profile
            .provider
            .as_deref()
            .is_some_and(|p| p != "openai-compat")
        || profile.env.keys().any(|k| k != "HARNESS_BASE_URL")
        || profile.env_passthrough.as_ref().is_some_and(|names| {
            names
                .iter()
                .any(|k| !["PATH", "LANG", "HARNESS_API_KEY"].contains(&k.as_str()))
        })
    {
        bail!(
            "soulsteads requires an explicit model, openai-compat provider, no args, and only its documented environment names"
        );
    }
    let endpoint = profile
        .env
        .get("HARNESS_BASE_URL")
        .context("soulsteads requires a static HARNESS_BASE_URL in the approved profile")?;
    let url = reqwest::Url::parse(endpoint).context("invalid soulsteads endpoint")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (authenticated(profile) && url.scheme() != "https")
    {
        bail!(
            "soulsteads endpoint must be HTTP(S) without userinfo/query/fragment; authentication requires HTTPS"
        );
    }
    Ok(())
}

fn authenticated(profile: &RunnerProfile) -> bool {
    profile
        .env_passthrough
        .iter()
        .flatten()
        .any(|k| k == "HARNESS_API_KEY")
}

// JSON basic-string escaping is valid TOML too, except TOML also forbids raw DEL.
fn quoted(value: &str) -> String {
    serde_json::to_string(value)
        .expect("string")
        .replace('\u{7f}', "\\u007f")
}

pub(super) fn config(profile: &RunnerProfile, mode: &str, workspace: &Path, agent: &str) -> String {
    let tools = if mode == "workspace-write" {
        "[\"read_file\", \"list_dir\", \"write_file\"]"
    } else {
        "[\"read_file\", \"list_dir\"]"
    };
    format!(
        "[agent]\nid = {}\n[provider]\nkind = \"openai-compat\"\nmodel = {}\nbase_url_env = \"HARNESS_BASE_URL\"\n{}[grants]\nworkspace = {}\ntools = {}\n[limits]\ndeadline_secs = {}\nmax_model_response_bytes = {}\ntool_output_max_bytes = {}\nmax_write_bytes = {}\n",
        quoted(agent),
        quoted(profile.model.as_deref().unwrap_or("")),
        if authenticated(profile) {
            "api_key_env = \"HARNESS_API_KEY\"\n"
        } else {
            ""
        },
        quoted(&workspace.to_string_lossy()),
        tools,
        profile.timeout_seconds,
        profile.max_output_bytes,
        profile.max_output_bytes.min(32768),
        MAX_ARTIFACT_BYTES,
    )
}

fn text<'a>(event: &'a Value, key: &str) -> Result<&'a str> {
    event[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("invalid soulsteads event string")
}
fn number(event: &Value, key: &str) -> Result<u64> {
    event[key]
        .as_u64()
        .context("invalid soulsteads event counter")
}
fn optional_text(event: &Value, key: &str) -> Result<()> {
    if !event.get(key).is_some_and(|v| v.is_null() || v.is_string()) {
        bail!("invalid soulsteads optional event string");
    }
    Ok(())
}

/// Diagnostic only for a native deadline during the first model request. Other
/// shapes stay generic; this never supplies output or evidence for adoption.
/// The caller requires bounded capture and EOF before consulting this helper.
pub(super) fn timeout_diagnostic(
    stdout: &[u8],
    code: Option<i32>,
    agent: &str,
    model: &str,
) -> Option<&'static str> {
    if code != Some(5) {
        return None;
    }
    let stdout = std::str::from_utf8(stdout).ok()?;
    if !stdout.ends_with('\n') {
        return None;
    }
    let mut lines = stdout.lines();
    let mut events = Vec::with_capacity(4);
    for _ in 0..4 {
        events.push(serde_json::from_str::<Value>(lines.next()?).ok()?);
    }
    if lines.next().is_some() {
        return None;
    }
    let session = text(&events[0], "session_id").ok()?;
    let run = text(&events[1], "run_id").ok()?;
    for (index, event) in events.iter().enumerate() {
        if number(event, "seq").ok()? != index as u64 + 1
            || text(event, "session_id").ok()? != session
            || (index > 0 && text(event, "run_id").ok()? != run)
        {
            return None;
        }
        number(event, "ts_ms").ok()?;
    }
    let [session, start, request, terminal] = events.as_slice() else {
        return None;
    };
    let system_hash = text(start, "system_sha256").ok()?;
    if session["type"] != "session_started"
        || session["agent_id"] != agent
        || session["model"] != model
        || session.get("run_id") != Some(&Value::Null)
        || session["resumed"] != false
        || session["repaired_tail"] != false
        || start["type"] != "run_started"
        || system_hash.len() != 64
        || !system_hash.bytes().all(|b| b.is_ascii_hexdigit())
        || request["type"] != "model_request"
        || number(request, "turn").ok()? != 1
        || terminal["type"] != "run_finished"
        || terminal["status"] != "timed_out"
        || terminal["reason"] != "deadline_exceeded"
        || terminal.get("final_text") != Some(&Value::Null)
        || number(terminal, "turns").ok()? != 1
        || number(terminal, "tool_calls").ok()? != 0
    {
        return None;
    }
    number(request, "messages").ok()?;
    number(request, "tools").ok()?;
    Some("native timed_out/deadline_exceeded")
}

/// Accept one new session/run and one completed terminal; never salvage a final
/// message from a broken stream. Unknown events require explicit adapter review.
pub(super) fn parse(stdout: &str, agent: &str, model: &str, mode: &str) -> Result<Parsed> {
    let mut session = String::new();
    let mut run = String::new();
    let mut final_text = None;
    let mut last_message = Value::Null;
    let mut requested_turn = 0;
    let mut responded_turn = 0;
    let mut response_tools = 0;
    let mut proposed_this_turn = 0;
    let mut assistant_seen = false;
    let mut tool_events = 0;
    // Local protocol validation only, not a durable tool authority/effect ledger.
    let mut calls: BTreeMap<String, (String, String, Value, bool, bool)> = BTreeMap::new();
    for (index, line) in stdout.lines().enumerate() {
        let event: Value = serde_json::from_str(line).context("malformed soulsteads JSONL")?;
        if final_text.is_some() || number(&event, "seq")? != index as u64 + 1 {
            bail!("soulsteads duplicate/out-of-order event or data after terminal");
        }
        number(&event, "ts_ms")?;
        let kind = text(&event, "type")?;
        let sid = text(&event, "session_id")?;
        if index == 0 {
            if kind != "session_started"
                || text(&event, "agent_id")? != agent
                || text(&event, "model")? != model
                || event.get("run_id") != Some(&Value::Null)
                || event["resumed"] != false
                || event["repaired_tail"] != false
            {
                bail!("soulsteads session does not match this fresh invocation");
            }
            session = sid.to_owned();
            continue;
        }
        if sid != session {
            bail!("soulsteads session mismatch");
        }
        let rid = text(&event, "run_id")?;
        if index == 1 {
            if kind != "run_started" || text(&event, "system_sha256")?.len() != 64 {
                bail!("soulsteads run start missing");
            }
            run = rid.to_owned();
            continue;
        }
        if rid != run {
            bail!("soulsteads run mismatch");
        }
        match kind {
            "model_request" => {
                if requested_turn != responded_turn
                    || number(&event, "turn")? != requested_turn + 1
                    || (requested_turn > 0
                        && (!assistant_seen || proposed_this_turn != response_tools))
                    || calls.values().any(|c| !c.4)
                {
                    bail!("soulsteads incomplete turn");
                }
                number(&event, "messages")?;
                number(&event, "tools")?;
                requested_turn += 1;
                assistant_seen = false;
                proposed_this_turn = 0;
            }
            "model_response" => {
                if requested_turn != responded_turn + 1 || number(&event, "turn")? != requested_turn
                {
                    bail!("soulsteads response has no matching request");
                }
                number(&event, "content_bytes")?;
                response_tools = number(&event, "tool_calls")?;
                for key in ["finish_reason", "served_model"] {
                    optional_text(&event, key)?;
                }
                for key in ["prompt_tokens", "completion_tokens"] {
                    if !event.get(key).is_some_and(|v| v.is_null() || v.is_u64()) {
                        bail!("invalid soulsteads usage counter");
                    }
                }
                responded_turn += 1;
            }
            "assistant_message" => {
                if requested_turn == 0 || requested_turn != responded_turn || assistant_seen {
                    bail!("soulsteads message has no matching response");
                }
                optional_text(&event, "content")?;
                last_message = event["content"].clone();
                assistant_seen = true;
            }
            "tool_proposed" => {
                if !assistant_seen || proposed_this_turn >= response_tools {
                    bail!("soulsteads tool has no matching model response");
                }
                let name = text(&event, "name")?;
                let allowed = matches!(name, "read_file" | "list_dir")
                    || (mode == "workspace-write" && name == "write_file");
                if !allowed
                    || event["approval"] != "allow"
                    || !event["arguments"].is_object()
                    || event["effect"]
                        != if name == "write_file" {
                            "mutating"
                        } else {
                            "read_only"
                        }
                {
                    bail!("soulsteads tool exceeds admitted grants or requires approval");
                }
                let id = text(&event, "attempt_id")?.to_owned();
                let call = (
                    text(&event, "call_id")?.to_owned(),
                    name.to_owned(),
                    event["arguments"].clone(),
                    false,
                    false,
                );
                if calls.insert(id, call).is_some() {
                    bail!("duplicate soulsteads tool attempt");
                }
                proposed_this_turn += 1;
            }
            "tool_attempt" | "tool_result" => {
                let call = calls
                    .get_mut(text(&event, "attempt_id")?)
                    .context("unmatched soulsteads tool event")?;
                if call.0 != text(&event, "call_id")? || call.1 != text(&event, "name")? || call.4 {
                    bail!("soulsteads tool correlation mismatch");
                }
                if kind == "tool_attempt" {
                    if call.3 || event["arguments"] != call.2 {
                        bail!("invalid soulsteads tool attempt");
                    }
                    call.3 = true;
                    tool_events += 1;
                } else {
                    if !call.3
                        || event["status"] != "ok"
                        || !event["output"].is_string()
                        || !event["truncated"].is_boolean()
                    {
                        bail!("soulsteads tool failed or has uncertain effects");
                    }
                    call.4 = true;
                }
            }
            "run_finished" => {
                if event["status"] != "completed"
                    || event.get("reason") != Some(&Value::Null)
                    || requested_turn == 0
                    || requested_turn != responded_turn
                    || !assistant_seen
                    || response_tools != 0
                    || proposed_this_turn != 0
                    || calls.values().any(|c| !c.4)
                    || number(&event, "turns")? != responded_turn
                    || number(&event, "tool_calls")? != calls.len() as u64
                    || event["final_text"] != last_message
                {
                    bail!("soulsteads terminal is unsuccessful or incomplete");
                }
                final_text = Some(text(&event, "final_text")?.to_owned());
            }
            // Includes error, tool_interrupted, tool_denied, tool_approval,
            // resumed grant/instruction changes and unexpected config surfaces.
            _ => bail!("unsupported or unsuccessful soulsteads event"),
        }
    }
    Ok(Parsed {
        text: final_text.context("soulsteads completed terminal missing")?,
        session_id: Some(session),
        native_run_id: Some(run),
        served_model: None,
        usage: Value::Null,
        tool_events: Some(tool_events),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> RunnerProfile {
        serde_json::from_value(json!({
            "adapter":"soulsteads", "executable":"/opt/bin/soulsteads-harness",
            "model":"fixture-model", "provider":"openai-compat",
            "permission_mode":"workspace-write",
            "env":{"HARNESS_BASE_URL":"http://127.0.0.1:8000/v1"},
            "env_passthrough":[]
        }))
        .unwrap()
    }

    fn events() -> Vec<Value> {
        vec![
            json!({"type":"session_started","agent_id":"forge-test","model":"fixture-model","resumed":false,"repaired_tail":false}),
            json!({"type":"run_started","system_sha256":"a".repeat(64)}),
            json!({"type":"model_request","turn":1,"messages":2,"tools":3}),
            json!({"type":"model_response","turn":1,"content_bytes":0,"tool_calls":1,"finish_reason":"tool_calls","served_model":"observed-model","prompt_tokens":8,"completion_tokens":2}),
            json!({"type":"assistant_message","content":null}),
            json!({"type":"tool_proposed","attempt_id":"attempt-1","call_id":"call-1","name":"write_file","arguments":{"path":"note.txt","content":"hello"},"effect":"mutating","approval":"allow"}),
            json!({"type":"tool_attempt","attempt_id":"attempt-1","call_id":"call-1","name":"write_file","arguments":{"path":"note.txt","content":"hello"}}),
            json!({"type":"tool_result","attempt_id":"attempt-1","call_id":"call-1","name":"write_file","status":"ok","output":"written","truncated":false}),
            json!({"type":"model_request","turn":2,"messages":4,"tools":3}),
            json!({"type":"model_response","turn":2,"content_bytes":15,"tool_calls":0,"finish_reason":"stop","served_model":null,"prompt_tokens":null,"completion_tokens":null}),
            json!({"type":"assistant_message","content":"{\"answer\":\"42\"}"}),
            json!({"type":"run_finished","status":"completed","reason":null,"final_text":"{\"answer\":\"42\"}","turns":2,"tool_calls":1}),
        ]
    }

    fn stream(mut events: Vec<Value>) -> String {
        for (index, event) in events.iter_mut().enumerate() {
            event["seq"] = json!(index + 1);
            event["ts_ms"] = json!(1234);
            event["session_id"] = json!("session-1");
            event["run_id"] = if index == 0 {
                Value::Null
            } else {
                json!("run-1")
            };
        }
        events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    #[test]
    fn soulsteads_admission_and_generated_grants_are_closed() {
        let p = profile();
        p.validate("native").unwrap();
        assert_eq!(
            p.capabilities(),
            [
                "structured_output",
                "tool_use",
                "session_correlation",
                "workspace_confinement"
            ]
        );
        for field in ["args", "model", "provider", "env", "env_passthrough"] {
            let mut invalid = serde_json::to_value(&p).unwrap();
            invalid[field] = match field {
                "args" => json!(["--approve-ask"]),
                "model" => Value::Null,
                "provider" => json!("other-provider"),
                "env" => json!({"SOULSTEADS_HARNESS_CONFIG":"/outside/config.toml"}),
                _ => json!(["SOULSTEADS_HARNESS_CONFIG"]),
            };
            assert!(
                serde_json::from_value::<RunnerProfile>(invalid)
                    .unwrap()
                    .validate("native")
                    .is_err(),
                "{field}"
            );
        }
        let ro = config(
            &p,
            "read-only",
            Path::new("/isolated/workspace"),
            "forge-test",
        );
        let rw = config(
            &p,
            "workspace-write",
            Path::new("/isolated/workspace"),
            "forge-test",
        );
        assert!(ro.contains("tools = [\"read_file\", \"list_dir\"]"));
        assert!(!ro.contains("write_file"));
        assert!(rw.contains("tools = [\"read_file\", \"list_dir\", \"write_file\"]"));
        assert!(rw.contains("workspace = \"/isolated/workspace\""));
        assert!(rw.contains("id = \"forge-test\""));
        assert!(!rw.contains("mcp_servers") && !rw.contains("instructions_files"));
    }

    #[test]
    fn soulsteads_deadline_diagnostic_requires_complete_correlated_native_stream() {
        let mut deadline = events()[..3].to_vec();
        deadline.push(json!({"type":"run_finished","status":"timed_out","reason":"deadline_exceeded","final_text":null,"turns":1,"tool_calls":0}));
        let good = stream(deadline);
        let diagnostic = |output: &str, code| {
            timeout_diagnostic(output.as_bytes(), code, "forge-test", "fixture-model")
        };
        assert_eq!(
            diagnostic(&good, Some(5)),
            Some("native timed_out/deadline_exceeded")
        );
        for code in [None, Some(0), Some(1), Some(3)] {
            assert_eq!(diagnostic(&good, code), None);
        }
        for (from, to) in [
            ("forge-test", "other-agent"),
            ("fixture-model", "other-model"),
            ("\"seq\":4", "\"seq\":3"),
            ("\"resumed\":false", "\"resumed\":true"),
            ("\"repaired_tail\":false", "\"repaired_tail\":true"),
            ("deadline_exceeded", "private-provider-error"),
            ("timed_out", "completed"),
            ("\"turn\":1", "\"turn\":\"1\""),
            ("\"final_text\":null", "\"final_text\":\"private-output\""),
            ("\"tool_calls\":0", "\"tool_calls\":1"),
            ("\"turns\":1", "\"turns\":0"),
            ("model_request", "unsupported_event"),
        ] {
            let bad = good.replacen(from, to, 1);
            assert_ne!(bad, good, "{from}");
            assert_eq!(diagnostic(&bad, Some(5)), None, "{from}");
        }
        let mut rows: Vec<Value> = good
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for field in ["session_id", "run_id"] {
            let original = rows[3][field].clone();
            rows[3][field] = json!("foreign");
            let bad = rows
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            assert_eq!(diagnostic(&bad, Some(5)), None);
            rows[3][field] = original;
        }
        for bad in [
            good.trim_end().to_owned(),
            good[..good.len() - 2].to_owned(),
            good.clone() + "not json\n",
            good.clone() + good.lines().last().unwrap() + "\n",
            stream(events()),
        ] {
            assert_eq!(diagnostic(&bad, Some(5)), None);
        }
        assert_eq!(
            timeout_diagnostic(&[0xff], Some(5), "forge-test", "fixture-model"),
            None
        );
        assert!(parse(&good, "forge-test", "fixture-model", "workspace-write").is_err());
    }

    #[test]
    fn soulsteads_success_uses_only_correlated_terminal_text() {
        let output = stream(events());
        let parsed = parse(&output, "forge-test", "fixture-model", "workspace-write").unwrap();
        assert_eq!(parsed.text, "{\"answer\":\"42\"}");
        assert_eq!(parsed.session_id.as_deref(), Some("session-1"));
        assert_eq!(parsed.tool_events, Some(1));
        assert!(parse(&output, "other-agent", "fixture-model", "workspace-write").is_err());
        assert!(parse(&output, "forge-test", "other-model", "workspace-write").is_err());
        assert!(parse(&output, "forge-test", "fixture-model", "read-only").is_err());
    }

    #[test]
    fn soulsteads_endpoint_and_config_values_cannot_change_grants() {
        let mut p = profile();
        for endpoint in [
            "file:///etc/passwd",
            &format!("{}://{}:{}@localhost/v1", "http", "user", "password"),
            "http://localhost/v1?token=x",
            "http://localhost/v1#fragment",
        ] {
            p.env.insert("HARNESS_BASE_URL".into(), endpoint.into());
            assert!(p.validate("native").is_err());
        }
        p.env
            .insert("HARNESS_BASE_URL".into(), "https://model.example/v1".into());
        p.env_passthrough = Some(vec!["HARNESS_API_KEY".into()]);
        p.model = Some("model\"\n[grants]\ntools=[]\u{7f}".into());
        p.validate("native").unwrap();
        let config = config(&p, "read-only", Path::new("/workspace"), "forge-test");
        assert!(config.contains("api_key_env = \"HARNESS_API_KEY\""));
        assert_eq!(config.matches("\n[grants]\n").count(), 1);
        assert!(config.contains("model\\\"\\n[grants]\\ntools=[]\\u007f"));
        assert!(!config.contains("write_file"));
    }

    #[test]
    fn soulsteads_terminal_and_correlation_faults_fail_closed() {
        for status in [
            "failed",
            "cancelled",
            "timed_out",
            "limit_exceeded",
            "uncertain",
            "unknown",
        ] {
            let mut bad = events();
            bad.last_mut().unwrap()["status"] = json!(status);
            assert!(
                parse(
                    &stream(bad),
                    "forge-test",
                    "fixture-model",
                    "workspace-write"
                )
                .is_err(),
                "{status}"
            );
        }
        let good = stream(events());
        for (from, to) in [
            ("\"session-1\"", "null"),
            ("\"run-1\"", "null"),
            ("\"seq\":2,", "\"seq\":1,"),
            ("\"resumed\":false", "\"resumed\":true"),
            ("\"status\":\"ok\"", "\"status\":\"uncertain\""),
            ("\"type\":\"model_response\"", "\"type\":\"unknown_event\""),
        ] {
            let bad = good.replacen(from, to, 1);
            assert_ne!(bad, good, "fixture mutation must apply");
            assert!(
                parse(&bad, "forge-test", "fixture-model", "workspace-write").is_err(),
                "{from}"
            );
        }
        for index in [0, 1, 6, 7, 11] {
            let mut bad = events();
            bad.remove(index);
            assert!(
                parse(
                    &stream(bad),
                    "forge-test",
                    "fixture-model",
                    "workspace-write"
                )
                .is_err(),
                "missing {index}"
            );
        }
        for extra in [
            "not-json\n",
            "{\"type\":\"error\",\"code\":\"failed\"}\n",
            good.lines().last().unwrap(),
        ] {
            assert!(
                parse(
                    &(good.clone() + extra),
                    "forge-test",
                    "fixture-model",
                    "workspace-write"
                )
                .is_err()
            );
        }
    }
}

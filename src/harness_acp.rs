//! Agent Client Protocol (ACP) client transport for harness workloads.
//!
//! Forge launches an operator-approved ACP agent over stdio inside the usual
//! containment, performs `initialize` -> `session/new` -> one `session/prompt`
//! turn, captures streamed `session/update` notifications into a private log and
//! the final text, and answers every server-to-client request deterministically:
//!
//! * `session/request_permission` is decided by [`decide`] from the profile's
//!   `permission_mode` ceiling and the invocation workspace. It never selects an
//!   `allow_always` option and denies anything it cannot positively scope.
//! * filesystem and terminal client capabilities are not advertised; any other
//!   request gets JSON-RPC `method_not_found` rather than being left hanging.
//!
//! The ACP client only speaks the protocol. DBOS ownership (the step, the effect
//! intent/receipt, uncertain/RECOVERY_REQUIRED on interruption) stays in
//! `flow_effects`/`flow_runtime`; this module adds no retry, queue or session
//! resume. The turn is never replayed: a lost connection is an uncertain attempt.
use super::*;
use agent_client_protocol::{Agent, Client, ConnectionTo, JsonRpcMessage, Lines, Responder};
use agent_client_protocol::{UntypedMessage, schema::v1};
use futures::StreamExt;
use std::{collections::HashSet, sync::Mutex as StdMutex};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::watch,
};

const FRAME_LIMIT: usize = 2 * 1024 * 1024;
const LOG_LIMIT: usize = 8 * 1024 * 1024;
const MAX_DECISIONS: usize = 256;
const MAX_TOOL_CALLS: usize = 256;
/// After a cancel request the agent gets this long to settle before the caller kills it.
pub(super) const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// Everything the transport needs; no authority beyond what the caller derived.
pub(super) struct Drive {
    pub workspace: PathBuf,
    pub mode: String,
    pub prompt: String,
    pub max_output: usize,
    pub log: PathBuf,
    pub secrets: Vec<String>,
    pub cancel: watch::Receiver<bool>,
    pub exceeded: Arc<AtomicBool>,
    /// Live observations, readable by the caller even when the turn fails.
    pub observed: Arc<StdMutex<Driven>>,
}

/// What a completed (or partially observed) turn produced.
#[derive(Default, Clone)]
pub(super) struct Driven {
    pub text: String,
    pub session_id: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Value,
    pub served_model: Option<String>,
    pub tool_events: usize,
    /// Each tool call's kind and last reported status from `tool_call` and
    /// `tool_call_update` session updates (bounded; titles clipped, never raw input).
    pub tool_calls: Vec<Value>,
    pub decisions: Vec<Value>,
    pub updates: usize,
    pub agent: Value,
}

/// Append-only private streamed-update log (`updates.jsonl`), bounded and scrubbed.
pub(super) struct UpdateLog {
    file: Option<std::fs::File>,
    bytes: usize,
    secrets: Vec<String>,
}
impl UpdateLog {
    pub(super) fn open(path: &Path, secrets: Vec<String>) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok();
        Self {
            file,
            bytes: 0,
            secrets,
        }
    }
    pub(super) fn write(&mut self, kind: &str, body: &Value) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let line = scrub(
            &json!({"ms":chrono::Utc::now().timestamp_millis(),"kind":kind,"body":body})
                .to_string(),
            &self.secrets,
        );
        if self.bytes + line.len() + 1 > LOG_LIMIT {
            return;
        }
        self.bytes += line.len() + 1;
        let _ = writeln!(file, "{line}");
    }
}

fn kind_of(params: &Value) -> &str {
    params["toolCall"]["kind"].as_str().unwrap_or("other")
}

/// Symlink expansions allowed while resolving one path (Linux allows 40).
const MAX_SYMLINK_HOPS: u32 = 40;

/// True when `path` is absolute, contains no `..`, and its fully resolved location
/// lies inside the (resolved) workspace. Resolution walks the path one component
/// at a time and expands **every** symlink it meets, including dangling ones: a
/// symlink whose target does not resolve inside the workspace is refused even if
/// the target does not exist yet (an agent could otherwise create the target
/// later, or the edit could create it through the link). Components that do not
/// exist and are not symlinks are kept as written. More than 40 expansions (a
/// loop) refuse.
///
/// The decision is advisory about a moment in time: the agent performs the
/// operation afterwards, so a path can be replaced between decision and use (see
/// docs/harness.md, "ACP permission policy limits").
fn inside(workspace: &Path, path: &str) -> bool {
    use std::ffi::OsString;
    use std::path::Component;
    let path = Path::new(path);
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let Ok(root) = workspace.canonicalize() else {
        return false;
    };
    let mut resolved = PathBuf::from("/");
    let mut pending: std::collections::VecDeque<OsString> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_owned()),
            _ => None,
        })
        .collect();
    let mut hops = 0;
    while let Some(name) = pending.pop_front() {
        let next = resolved.join(&name);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                let Ok(target) = std::fs::read_link(&next) else {
                    return false;
                };
                if hops > MAX_SYMLINK_HOPS {
                    return false;
                }
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                // Splice the target's components in front of the remaining ones.
                // `..` is applied to the already-resolved prefix.
                let mut spliced = std::collections::VecDeque::new();
                for component in target.components() {
                    match component {
                        Component::Normal(part) => spliced.push_back(part.to_owned()),
                        Component::ParentDir => spliced.push_back(OsString::from("..")),
                        _ => {}
                    }
                }
                while let Some(part) = spliced.pop_back() {
                    pending.push_front(part);
                }
            }
            Ok(_) if name == ".." => {
                resolved.pop();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if name == ".." {
                    resolved.pop();
                } else {
                    resolved = next;
                }
            }
            Ok(_) => resolved = next,
            Err(_) => return false,
        }
    }
    resolved.starts_with(&root)
}

/// Deterministic permission policy. Returns the JSON-RPC `outcome` object and an
/// evidence record. Deny by default: only workspace-confined read/search (any
/// mode) and workspace-confined edit (`workspace-write` ceiling only) are allowed,
/// and only through a one-shot `allow_once` option the agent actually offered.
pub(super) fn decide(params: &Value, mode: &str, workspace: &Path) -> (Value, Value) {
    let kind = kind_of(params);
    let listed = params["toolCall"]["locations"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let locations: Vec<&str> = listed.iter().filter_map(|l| l["path"].as_str()).collect();
    let located = !locations.is_empty()
        && locations.len() == listed.len()
        && locations.iter().all(|p| inside(workspace, p));
    let allowed = match kind {
        "read" | "search" => located,
        "edit" => located && mode == "workspace-write",
        _ => false,
    };
    let options = params["options"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let pick = |wanted: &str| {
        options
            .iter()
            .find(|o| o["kind"] == wanted)
            .and_then(|o| o["optionId"].as_str())
    };
    let reason = match (kind, allowed) {
        ("read" | "search", true) => "workspace-confined read/search is allowed".to_owned(),
        ("edit", true) => "workspace-confined edit is allowed under workspace-write".to_owned(),
        ("read" | "search" | "edit", false) if !located => {
            "location missing or not confined to the workspace".to_owned()
        }
        ("edit", false) => "edit requires the workspace-write permission mode".to_owned(),
        (other, _) => format!(
            "kind {other} is denied by policy (only workspace read, search and edit are allowed)"
        ),
    };
    let (outcome, decision) = if allowed {
        match pick("allow_once") {
            Some(id) => (json!({"outcome":"selected","optionId":id}), "allowed_once"),
            None => (json!({"outcome":"cancelled"}), "cancelled_no_allow_option"),
        }
    } else {
        match pick("reject_once") {
            Some(id) => (json!({"outcome":"selected","optionId":id}), "denied"),
            None => (json!({"outcome":"cancelled"}), "cancelled"),
        }
    };
    let record = json!({
        "ms": chrono::Utc::now().timestamp_millis(),
        "tool_call_id": params["toolCall"]["toolCallId"],
        "title": params["toolCall"]["title"].as_str().map(|t| clipped(t, 200)),
        "kind": kind,
        "locations": locations,
        "mode": mode,
        "decision": decision,
        "reason": reason,
    });
    (outcome, record)
}

struct Shared {
    state: Arc<StdMutex<Driven>>,
    log: StdMutex<UpdateLog>,
    tools: StdMutex<HashSet<String>>,
    workspace: PathBuf,
    mode: String,
    max_output: usize,
    exceeded: Arc<AtomicBool>,
}

impl Driven {
    /// Record or refresh one tool call from a session update; later fields win.
    fn note_tool(&mut self, update: &Value) {
        let Some(id) = update["toolCallId"].as_str() else {
            return;
        };
        let at = match self.tool_calls.iter().position(|t| t["id"] == id) {
            Some(at) => at,
            None if self.tool_calls.len() < MAX_TOOL_CALLS => {
                self.tool_calls
                    .push(json!({"id":clipped(id,128),"kind":"other","status":"pending"}));
                self.tool_calls.len() - 1
            }
            None => return,
        };
        let entry = &mut self.tool_calls[at];
        for field in ["kind", "status"] {
            if let Some(value) = update[field].as_str() {
                entry[field] = json!(clipped(value, 64));
            }
        }
        if let Some(title) = update["title"].as_str() {
            entry["title"] = json!(clipped(title, 200));
        }
    }
}

impl Shared {
    fn update(&self, params: &Value) {
        let update = &params["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        let mut state = self.state.lock().expect("acp state");
        if state
            .session_id
            .as_deref()
            .is_some_and(|s| params["sessionId"].as_str() != Some(s))
        {
            return;
        }
        state.updates += 1;
        match kind {
            "agent_message_chunk" if update["content"]["type"] == "text" => {
                let chunk = update["content"]["text"].as_str().unwrap_or("");
                if state.text.len() + chunk.len() > self.max_output {
                    self.exceeded.store(true, Ordering::SeqCst);
                } else {
                    state.text.push_str(chunk);
                }
            }
            "tool_call_update" => state.note_tool(update),
            "tool_call" => {
                state.note_tool(update);
                if let Some(id) = update["toolCallId"].as_str()
                    && self.tools.lock().expect("acp tools").insert(id.to_owned())
                {
                    state.tool_events += 1;
                    // The final answer is the message after the last tool; earlier commentary is dropped.
                    state.text.clear();
                }
            }
            "usage_update" => state.usage = update.clone(),
            _ => {}
        }
        drop(state);
        // Thoughts are not retained; every other update is streamed to the private log.
        if kind != "agent_thought_chunk" {
            self.log.lock().expect("acp log").write(kind, update);
        }
    }
    fn permission(&self, params: &Value) -> Value {
        let (outcome, record) = decide(params, &self.mode, &self.workspace);
        self.log
            .lock()
            .expect("acp log")
            .write("permission", &record);
        let mut state = self.state.lock().expect("acp state");
        if state.decisions.len() < MAX_DECISIONS {
            state.decisions.push(record);
        }
        outcome
    }
}

/// Bounded newline-delimited stdout reader. Replies are queued behind complete
/// buffered notifications so streamed chunks written just before the terminal
/// reply are applied before the turn completes.
fn incoming(
    stdout: ChildStdout,
) -> impl futures::Stream<Item = std::io::Result<String>> + Send + 'static {
    futures::stream::try_unfold(
        (
            BufReader::new(stdout),
            std::collections::VecDeque::<String>::new(),
        ),
        |(mut reader, mut replies)| async move {
            loop {
                if !reader.buffer().contains(&b'\n')
                    && let Some(line) = replies.pop_front()
                {
                    return Ok(Some((line, (reader, replies))));
                }
                let mut bytes = Vec::new();
                let count = (&mut reader)
                    .take(FRAME_LIMIT as u64 + 1)
                    .read_until(b'\n', &mut bytes)
                    .await?;
                if count == 0 {
                    return Ok(replies.pop_front().map(|line| (line, (reader, replies))));
                }
                if bytes.len() > FRAME_LIMIT {
                    return Err(std::io::Error::other("ACP frame exceeded limit"));
                }
                let line = String::from_utf8(bytes).map_err(std::io::Error::other)?;
                if line.trim().is_empty() {
                    continue;
                }
                let reply = serde_json::from_str::<Value>(&line).is_ok_and(|v| {
                    v.is_object() && v.get("method").is_none() && v.get("id").is_some()
                });
                if reply {
                    if replies.len() >= 128 {
                        return Err(std::io::Error::other("ACP reply queue full"));
                    }
                    replies.push_back(line);
                } else {
                    return Ok(Some((line, (reader, replies))));
                }
            }
        },
    )
}

fn outgoing(stdin: ChildStdin) -> impl futures::Sink<String, Error = std::io::Error> + Send {
    futures::sink::unfold(stdin, |mut stdin, line: String| async move {
        if line.len() > FRAME_LIMIT {
            return Err(std::io::Error::other("ACP frame exceeded limit"));
        }
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok::<_, std::io::Error>(stdin)
    })
}

fn typed(method: &str, params: Value) -> Result<UntypedMessage> {
    macro_rules! typed {
        ($request:ty) => {{
            let request: $request = serde_json::from_value(params)?;
            request.to_untyped_message().map_err(anyhow::Error::from)
        }};
    }
    match method {
        "initialize" => typed!(v1::InitializeRequest),
        "session/new" => typed!(v1::NewSessionRequest),
        "session/prompt" => typed!(v1::PromptRequest),
        _ => bail!("unsupported ACP request"),
    }
}

async fn call(connection: &ConnectionTo<Agent>, method: &str, params: Value) -> Result<Value> {
    connection
        .send_request(typed(method, params)?)
        .block_task()
        .await
        .map_err(|error| anyhow::anyhow!("ACP {method} failed: {}", error.message))
}

/// Run one prompt turn to completion. `stdin`/`stdout` belong to the contained
/// agent process, whose lifetime the caller owns (the caller kills it afterwards).
pub(super) async fn drive(stdin: ChildStdin, stdout: ChildStdout, d: Drive) -> Result<Driven> {
    let shared = Arc::new(Shared {
        state: d.observed.clone(),
        log: StdMutex::new(UpdateLog::open(&d.log, d.secrets.clone())),
        tools: StdMutex::new(HashSet::new()),
        workspace: d.workspace.clone(),
        mode: d.mode.clone(),
        max_output: d.max_output,
        exceeded: d.exceeded.clone(),
    });
    let flow: Arc<StdMutex<Option<Result<()>>>> = Arc::new(StdMutex::new(None));
    let (notify, requests, run) = (shared.clone(), shared.clone(), shared.clone());
    let slot = flow.clone();
    let mut cancel = d.cancel.clone();
    let workspace = d.workspace.clone();
    let prompt = d.prompt.clone();
    let transport = Lines::new(Box::pin(outgoing(stdin)), incoming(stdout).boxed());
    let connected = Client
        .builder()
        .on_receive_notification(
            async move |message: UntypedMessage, _cx| {
                if message.method == "session/update" {
                    notify.update(&message.params);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |message: UntypedMessage, responder: Responder<Value>, _cx| {
                if message.method == "session/request_permission" {
                    let outcome = requests.permission(&message.params);
                    return responder.respond(json!({"outcome":outcome}));
                }
                requests.log.lock().expect("acp log").write(
                    "client_request_refused",
                    &json!({"method":clipped(&message.method, 200)}),
                );
                responder.respond_with_error(agent_client_protocol::Error::method_not_found())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |connection| {
            let result = async {
                let init = call(
                    &connection,
                    "initialize",
                    json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"forge","title":"Forge","version":env!("CARGO_PKG_VERSION")}}),
                )
                .await?;
                if init["protocolVersion"] != 1 {
                    bail!("ACP agent did not negotiate protocol version 1");
                }
                run.state.lock().expect("acp state").agent = json!({"info":init["agentInfo"],"capabilities":init["agentCapabilities"],"protocol":1});
                let created = call(
                    &connection,
                    "session/new",
                    json!({"cwd":workspace,"mcpServers":[]}),
                )
                .await?;
                let session = created["sessionId"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256)
                    .context("ACP did not return a session ID")?
                    .to_owned();
                run.state.lock().expect("acp state").session_id = Some(session.clone());
                let turn = call(
                    &connection,
                    "session/prompt",
                    json!({"sessionId":session,"prompt":[{"type":"text","text":prompt}]}),
                );
                tokio::pin!(turn);
                let reply = loop {
                    tokio::select! {
                        reply = &mut turn => break reply?,
                        changed = cancel.changed() => {
                            if changed.is_ok() && *cancel.borrow() {
                                let _ = connection.send_notification(UntypedMessage::new(
                                    "session/cancel",
                                    json!({"sessionId":session}),
                                )?);
                                run.log.lock().expect("acp log").write("cancel_sent", &json!({}));
                                let _ = tokio::time::timeout(CANCEL_GRACE, &mut turn).await;
                                bail!("ACP turn cancelled");
                            }
                        }
                    }
                };
                let mut state = run.state.lock().expect("acp state");
                state.stop_reason = reply["stopReason"].as_str().map(str::to_owned);
                if !reply["usage"].is_null() {
                    state.usage = reply["usage"].clone();
                }
                state.served_model = reply["_meta"]["model"]
                    .as_str()
                    .or_else(|| reply["_meta"]["servedModel"].as_str())
                    .map(str::to_owned);
                Ok(())
            }
            .await;
            *slot.lock().expect("acp flow") = Some(result);
            Ok(())
        })
        .await;
    let outcome = flow.lock().expect("acp flow").take();
    let driven = shared.state.lock().expect("acp state").clone();
    match (outcome, connected) {
        (Some(Err(error)), _) => Err(error),
        (Some(Ok(())), _) => {
            if d.exceeded.load(Ordering::SeqCst) {
                bail!("ACP output exceeded the profile bound");
            }
            if driven.stop_reason.as_deref() != Some("end_turn") {
                bail!(
                    "ACP turn did not end normally (stopReason {})",
                    driven.stop_reason.as_deref().unwrap_or("missing")
                );
            }
            Ok(driven)
        }
        (None, Err(error)) => Err(anyhow::anyhow!("ACP connection closed: {error}")),
        (None, Ok(())) => bail!("ACP connection closed before the turn completed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: &str, paths: &[&str], options: &[(&str, &str)]) -> Value {
        json!({"sessionId":"s","toolCall":{"toolCallId":"t","title":"x","kind":kind,"locations":paths.iter().map(|p| json!({"path":p})).collect::<Vec<_>>()},
            "options":options.iter().map(|(id,k)| json!({"optionId":id,"name":id,"kind":k})).collect::<Vec<_>>()})
    }
    const STANDARD: [(&str, &str); 3] = [
        ("always", "allow_always"),
        ("allow", "allow_once"),
        ("reject", "reject_once"),
    ];

    #[test]
    fn permission_policy_is_deny_by_default_and_bounded_by_the_ceiling() {
        let ws = tempfile::tempdir().unwrap();
        let w = ws.path().canonicalize().unwrap();
        let inside_file = w.join("a.txt").to_string_lossy().into_owned();
        let decide_for = |kind: &str, paths: &[&str], mode: &str| {
            let (outcome, record) = decide(&request(kind, paths, &STANDARD), mode, &w);
            (outcome, record["decision"].as_str().unwrap().to_owned())
        };
        for mode in ["read-only", "workspace-write"] {
            let (outcome, d) = decide_for("read", &[&inside_file], mode);
            assert_eq!(
                (outcome["optionId"].as_str(), d.as_str()),
                (Some("allow"), "allowed_once")
            );
        }
        assert_eq!(
            decide_for("edit", &[&inside_file], "workspace-write").1,
            "allowed_once"
        );
        let file = inside_file.as_str();
        for (kind, paths, mode) in [
            ("edit", vec![file], "read-only"),
            ("execute", vec![file], "workspace-write"),
            ("fetch", vec![], "workspace-write"),
            ("delete", vec![file], "workspace-write"),
            ("read", vec!["/etc/passwd"], "workspace-write"),
            ("read", vec![], "workspace-write"),
            ("edit", vec!["/tmp/outside.txt"], "workspace-write"),
            ("other", vec![file], "workspace-write"),
        ] {
            let (outcome, d) = decide_for(kind, &paths, mode);
            assert_eq!(d, "denied", "{kind} {paths:?} {mode}");
            assert_eq!(outcome["optionId"], "reject");
        }
        let traversal = format!("{}/../escape", w.display());
        assert_eq!(
            decide_for("edit", &[&traversal], "workspace-write").1,
            "denied"
        );
        std::os::unix::fs::symlink("/etc", w.join("link")).unwrap();
        let through = w.join("link/passwd").to_string_lossy().into_owned();
        assert_eq!(
            decide_for("read", &[&through], "workspace-write").1,
            "denied"
        );
        let (outcome, record) = decide(
            &request("execute", &[], &[("a", "allow_always")]),
            "workspace-write",
            &w,
        );
        assert_eq!(outcome, json!({"outcome":"cancelled"}));
        assert_eq!(record["decision"], "cancelled");
        let (outcome, _) = decide(
            &request("read", &[&inside_file], &[("a", "allow_always")]),
            "read-only",
            &w,
        );
        assert_eq!(
            outcome,
            json!({"outcome":"cancelled"}),
            "allow_always is never selected"
        );
    }

    #[test]
    fn symlinks_are_refused_unless_their_target_resolves_inside_the_workspace() {
        use std::os::unix::fs::symlink;
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let ws = base.path().join("ws");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let w = ws.canonicalize().unwrap();
        let p = |rel: &str| w.join(rel).to_string_lossy().into_owned();
        let allowed = |rel: &str| inside(&w, &p(rel));
        // Plain paths, existing or not.
        assert!(allowed("sub/new.txt"));
        assert!(allowed("a/b/c/new.txt"));
        // Dangling symlink to a nonexistent path outside the workspace.
        symlink(outside.join("nodir/new"), w.join("dangling")).unwrap();
        assert!(!allowed("dangling"));
        assert!(!allowed("dangling/child"));
        // Nested: dangling link in a subdirectory, and a link chain ending outside.
        symlink(outside.join("later"), w.join("sub/dangling2")).unwrap();
        assert!(!allowed("sub/dangling2"));
        symlink(w.join("sub/dangling2"), w.join("chain")).unwrap();
        assert!(!allowed("chain"));
        // Relative dangling link climbing out of the workspace.
        symlink("../outside/missing", w.join("rel")).unwrap();
        assert!(!allowed("rel"));
        symlink("../../outside/missing", w.join("sub/rel2")).unwrap();
        assert!(!allowed("sub/rel2/x"));
        // Existing outside directory.
        symlink(&outside, w.join("dirlink")).unwrap();
        assert!(!allowed("dirlink/secret"));
        // A dangling link whose target is inside the workspace is fine.
        symlink(w.join("sub/will-exist"), w.join("inner")).unwrap();
        assert!(allowed("inner"));
        symlink("sub/rel-inner", w.join("inner2")).unwrap();
        assert!(allowed("inner2"));
        // Loops refuse instead of spinning.
        symlink("loop-b", w.join("loop-a")).unwrap();
        symlink("loop-a", w.join("loop-b")).unwrap();
        assert!(!allowed("loop-a"));
        // The workspace root itself may be reached through a symlink.
        let root_link = base.path().join("root-link");
        symlink(&ws, &root_link).unwrap();
        assert!(inside(
            &root_link,
            &root_link.join("sub/x").to_string_lossy()
        ));
        assert!(!inside(
            &root_link,
            &root_link.join("dirlink/x").to_string_lossy()
        ));
        // Through decide(): an edit through the dangling link is denied.
        let (_, record) = decide(
            &request("edit", &[&p("dangling")], &STANDARD),
            "workspace-write",
            &w,
        );
        assert_eq!(record["decision"], "denied");
    }
}

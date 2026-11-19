//! Pi coding-agent transport: `pi --mode rpc` newline-delimited JSON commands and
//! events (not JSON-RPC), following the launch and parse behavior of the Messaging
//! Pi adapter. One `prompt` command is sent and the session-level `agent_settled`
//! event closes the turn. Pi has no native approval prompts; the adapter derives
//! its tool allowlist from `permission_mode` and answers any extension dialog with
//! `cancelled` (extensions are disabled anyway). No session resume is used.
use super::{
    acp::{CANCEL_GRACE, Drive, Driven, UpdateLog},
    *,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{ChildStdin, ChildStdout},
};

const FRAME_LIMIT: u64 = 8 * 1024 * 1024;

/// Fixed, adapter-owned launch arguments. `--tools` is the permission ceiling:
/// no shell tool is ever granted, and writing requires the `workspace-write` ceiling.
pub(super) fn argv(mode: &str, provider: Option<&str>, model: Option<&str>) -> Vec<String> {
    let tools = if mode == "workspace-write" {
        "read,grep,find,ls,edit,write"
    } else {
        "read,grep,find,ls"
    };
    let mut argv: Vec<String> = [
        "--mode",
        "rpc",
        "--no-themes",
        "--no-session",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-mcp",
        "--tools",
        tools,
    ]
    .map(String::from)
    .into();
    if let Some(provider) = provider {
        argv.extend(["--provider".into(), provider.into()]);
    }
    if let Some(model) = model {
        argv.extend(["--model".into(), model.into()]);
    }
    argv
}

/// Runtime directories replaced by an empty tmpfs inside the sandbox.
pub(crate) const HIDDEN_RUNTIME_DIRS: &[&str] = &["/run/user", "/run/dbus"];

/// `bwrap` arguments wrapping a Pi launch: the whole filesystem read-only, the
/// invoking user's HOME and the session/system D-Bus runtime directories (`/run/user`,
/// `/run/dbus`) hidden, a private `/tmp`, and read-write access only to the
/// invocation workspace and Pi's isolated HOME. The Pi agent directory (the provider
/// credentials and trust files Pi must read and lock) is re-exposed read-write, so
/// it must be a dedicated directory; the executable's own directory is read-only. Reads elsewhere on the filesystem remain possible; this confines
/// writes and hides the user's HOME, it is not a complete read jail. The network
/// is not unshared because Pi must reach its model endpoint.
pub(crate) fn bwrap_argv(
    executable: &str,
    workspace: &Path,
    pi_home: &Path,
    agent_dir: Option<&Path>,
    pi_args: &[String],
) -> Vec<String> {
    let text = |p: &Path| p.to_string_lossy().into_owned();
    let mut argv: Vec<String> = ["--die-with-parent", "--new-session", "--ro-bind", "/", "/"]
        .map(String::from)
        .into();
    argv.extend([
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
    ]);
    argv.extend(["--tmpfs".into(), "/tmp".into()]);
    // The user session's runtime directory holds the D-Bus session socket, the keyring
    // and agent sockets; the system bus socket lives in /run/dbus. Hide both (only
    // those: DNS and other services under /run stay reachable so Pi can resolve its
    // model endpoint). A directory that does not exist cannot be a mount point.
    for dir in HIDDEN_RUNTIME_DIRS {
        if Path::new(dir).is_dir() {
            argv.extend(["--tmpfs".into(), (*dir).into()]);
        }
    }
    if let Some(user_home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        argv.extend(["--tmpfs".into(), text(Path::new(&user_home))]);
    }
    if let Some(dir) = Path::new(executable).parent() {
        argv.extend(["--ro-bind".into(), text(dir), text(dir)]);
    }
    for writable in agent_dir.into_iter().chain([workspace, pi_home]) {
        argv.extend(["--bind".into(), text(writable), text(writable)]);
    }
    argv.extend([
        "--chdir".into(),
        text(workspace),
        "--".into(),
        executable.into(),
    ]);
    argv.extend(pi_args.iter().cloned());
    argv
}

fn text_of(message: &Value) -> String {
    message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(super) async fn drive(stdin: ChildStdin, stdout: ChildStdout, d: Drive) -> Result<Driven> {
    let mut out = Driven {
        agent: json!({"protocol":"pi-rpc"}),
        ..Driven::default()
    };
    let result = turn(stdin, stdout, &d, &mut out).await;
    *d.observed.lock().expect("pi observed") = out.clone();
    result.map(|()| out)
}

async fn turn(
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    d: &Drive,
    out: &mut Driven,
) -> Result<()> {
    let mut log = UpdateLog::open(&d.log, d.secrets.clone());
    let mut cancel = d.cancel.clone();
    let mut reader = BufReader::new(stdout);
    let mut started = false;
    let mut current = String::new();
    let mut final_text = String::new();
    let mut prelude = 0usize;
    let command = json!({"id":"forge-prompt","type":"prompt","message":d.prompt});
    stdin.write_all(command.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    let mut aborting: Option<tokio::time::Instant> = None;
    loop {
        let mut bytes = Vec::new();
        let count = {
            let mut limited = (&mut reader).take(FRAME_LIMIT + 1);
            let read = limited.read_until(b'\n', &mut bytes);
            tokio::pin!(read);
            loop {
                tokio::select! {
                    n = &mut read => break n?,
                    changed = cancel.changed(), if aborting.is_none() => {
                        if changed.is_ok() && *cancel.borrow() {
                            stdin.write_all(b"{\"type\":\"abort\"}\n").await?;
                            stdin.flush().await?;
                            log.write("cancel_sent", &json!({}));
                            aborting = Some(tokio::time::Instant::now() + CANCEL_GRACE);
                        }
                    }
                    _ = async { match aborting { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => {
                        bail!("Pi turn cancelled");
                    }
                }
            }
        };
        if count == 0 {
            bail!("Pi exited before the turn settled");
        }
        if bytes.len() as u64 > FRAME_LIMIT {
            bail!("Pi frame exceeded limit");
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let event: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) if !started && bytes.first() != Some(&b'{') => {
                prelude += bytes.len();
                if prelude > 65536 {
                    bail!("Pi startup prelude exceeded limit");
                }
                continue;
            }
            Err(_) => bail!("Pi sent a malformed protocol record"),
        };
        started = true;
        out.updates += 1;
        let kind = event["type"].as_str().unwrap_or("");
        match kind {
            "response" if event["id"] == "forge-prompt" => {
                if event["success"] != true {
                    bail!(
                        "Pi rejected the prompt: {}",
                        clipped(event["error"].as_str().unwrap_or("unknown error"), 300)
                    );
                }
                if event["data"]["disposition"] == "handled" {
                    bail!("Pi handled the prompt without starting a run");
                }
                log.write("prompt_accepted", &event["data"]);
            }
            "response" => {}
            "message_start" if event["message"]["role"] == "assistant" => current.clear(),
            "message_update" => {
                let delta = &event["assistantMessageEvent"];
                if delta["type"] == "text_delta"
                    && let Some(chunk) = delta["delta"].as_str()
                {
                    if current.len() + chunk.len() > d.max_output {
                        d.exceeded.store(true, Ordering::SeqCst);
                        bail!("Pi output exceeded the profile bound");
                    }
                    current.push_str(chunk);
                    log.write("agent_message_chunk", &json!({"text":chunk}));
                }
            }
            "message_end" if event["message"]["role"] == "assistant" => {
                let message = &event["message"];
                match message["stopReason"].as_str() {
                    Some("error") => bail!(
                        "Pi provider failed: {}",
                        clipped(message["errorMessage"].as_str().unwrap_or("unknown"), 300)
                    ),
                    Some("aborted") if aborting.is_none() => bail!("Pi run aborted unexpectedly"),
                    _ => {}
                }
                let text = text_of(message);
                final_text = if text.is_empty() {
                    current.clone()
                } else {
                    text
                };
                if let Some(model) = message["model"].as_str() {
                    out.served_model = Some(model.to_owned());
                }
                if !message["usage"].is_null() {
                    out.usage = message["usage"].clone();
                }
                log.write(
                    "message_end",
                    &json!({"stopReason":message["stopReason"],"model":message["model"]}),
                );
            }
            "tool_execution_start" => {
                out.tool_events += 1;
                log.write(
                    "tool_call",
                    &json!({"tool":event["toolName"],"id":event["toolCallId"]}),
                );
            }
            "tool_execution_end" => log.write(
                "tool_call_update",
                &json!({"id":event["toolCallId"],"isError":event["isError"]}),
            ),
            "extension_ui_request" => {
                if let Some(id) = event["id"].as_str()
                    && matches!(
                        event["method"].as_str(),
                        Some("select" | "confirm" | "input" | "editor")
                    )
                {
                    let reply = json!({"type":"extension_ui_response","id":id,"cancelled":true});
                    stdin.write_all(reply.to_string().as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                    let record = json!({"ms":chrono::Utc::now().timestamp_millis(),"kind":event["method"],"title":event["title"].as_str().map(|t| clipped(t, 200)),"mode":d.mode,"decision":"cancelled"});
                    log.write("permission", &record);
                    if out.decisions.len() < 256 {
                        out.decisions.push(record);
                    }
                }
            }
            "error" => bail!(
                "Pi reported an error: {}",
                clipped(event["message"].as_str().unwrap_or("unknown"), 300)
            ),
            "agent_settled" => {
                if aborting.is_some() || event["aborted"] == true {
                    bail!("Pi turn was aborted");
                }
                break;
            }
            _ => {}
        }
    }
    if final_text.trim().is_empty() {
        bail!("Pi produced no final assistant text");
    }
    out.text = final_text;
    out.stop_reason = Some("end_turn".into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_arguments_derive_tools_from_the_ceiling_and_never_grant_a_shell() {
        let read = argv("read-only", Some("local"), Some("m1"));
        let write = argv("workspace-write", None, None);
        for args in [&read, &write] {
            assert!(args.windows(2).any(|w| w == ["--mode", "rpc"]));
            assert!(args.contains(&"--no-extensions".to_owned()));
            assert!(!args.iter().any(|a| a.contains("bash")));
        }
        assert!(
            read.windows(2)
                .any(|w| w == ["--tools", "read,grep,find,ls"])
        );
        assert!(write.iter().any(|a| a == "read,grep,find,ls,edit,write"));
        assert!(read.windows(2).any(|w| w == ["--provider", "local"]));
        assert!(read.windows(2).any(|w| w == ["--model", "m1"]));
    }
}

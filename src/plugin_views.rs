//! Configured executable discovery uses the public cached types catalog;
//! validation uses exactly the registration operation exposed over MCP.
use crate::{
    client::ApiClient,
    operations::{Plugins, json_input},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

fn plugin_type(kind: &str) -> Result<()> {
    if !kind.starts_with("plugin.") {
        bail!("Expected a plugin.NAME type; use `plugins list` to discover types");
    }
    Ok(())
}

fn absolute(source: &std::path::Path) -> Result<String> {
    Ok(std::path::absolute(source)
        .context("plugin source path is unusable")?
        .to_string_lossy()
        .into_owned())
}

/// One-line operator summary of an installed plugin.
fn summary(plugin: &Value) -> String {
    let mut line = format!(
        "{} {} [{}] {}",
        plugin["id"].as_str().unwrap_or("?"),
        plugin["version"].as_str().unwrap_or("?"),
        plugin["state"].as_str().unwrap_or("?"),
        serde_json::to_string(&plugin["description"]).unwrap_or_default()
    );
    if let Some(problem) = plugin["problem"].as_str() {
        line.push_str(&format!("\n    problem: {problem}"));
    }
    let kinds: Vec<String> = plugin["contributions"]
        .as_array()
        .map(|c| {
            c.iter()
                .map(|c| {
                    format!(
                        "{}:{}",
                        c["kind"].as_str().unwrap_or("?"),
                        c["id"].as_str().unwrap_or("?")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if !kinds.is_empty() {
        line.push_str(&format!("\n    contributes: {}", kinds.join(", ")));
    }
    line
}

fn catalog(snapshot: &Value) -> Result<Value> {
    let types = snapshot["types"]
        .as_object()
        .context("Forge snapshot has no types catalog")?;
    Ok(Value::Object(
        types
            .iter()
            .filter(|(key, _)| key.starts_with("plugin."))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    ))
}

pub(crate) async fn plugins(client: &ApiClient, command: Plugins) -> Result<()> {
    match command {
        Plugins::List => {
            let result = catalog(&client.invoke("snapshot", json!({})).await?)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Plugins::Show {
            r#type: kind,
            json,
            check,
        } if !kind.starts_with("plugin.") => {
            let result = client
                .invoke("plugin_show", json!({"id":kind,"check":check}))
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", summary(&result));
                if let Some(health) = result.get("health") {
                    println!("    health: {}", serde_json::to_string(health)?);
                }
            }
        }
        Plugins::Show {
            r#type: kind, json, ..
        } => {
            plugin_type(&kind)?;
            let types = catalog(&client.invoke("snapshot", json!({})).await?)?;
            let metadata = types
                .get(&kind)
                .context("Plugin type is not configured in this service; use `plugins list`")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"type":kind,"description":metadata["description"],"schema":metadata["schema"]})
                    )?
                );
            } else {
                // Metadata is external text; quote control characters for terminals.
                println!(
                    "Type: {}\nDescription: {}\nInput schema:\n{}\nValidate input: plugins validate {} --input input.json\nUse this type as a workflow step action; inspect outcomes with runs show RUN_ID.",
                    serde_json::to_string(&kind)?,
                    serde_json::to_string(&metadata["description"])?,
                    serde_json::to_string_pretty(&metadata["schema"])?,
                    serde_json::to_string(&kind)?
                );
            }
        }
        Plugins::Validate {
            r#type: kind,
            input,
            json,
        } => {
            plugin_type(&kind)?;
            let result = client
                .invoke(
                    "validate",
                    json!({"definition":{"type":kind,"input":json_input(&input)?}}),
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "Valid input for {} under registration policy. Nothing stored or executed.\nUse this type and input in work register or a workflow step; execution requires execute authority.",
                    serde_json::to_string(&kind)?
                );
            }
        }
        Plugins::Installed { json } => {
            let result = client.invoke("plugin_list", json!({})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.as_array().is_none_or(Vec::is_empty) {
                println!("No installed plugins. Install one with: plugins install DIR");
            } else {
                for plugin in result.as_array().into_iter().flatten() {
                    println!("{}", summary(plugin));
                }
            }
        }
        Plugins::Check { source } => {
            let result = client
                .invoke(
                    "plugin_install",
                    json!({"source":absolute(&source)?,"dry_run":true}),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Plugins::Install { source } => {
            let result = client
                .invoke(
                    "plugin_install",
                    json!({"source":absolute(&source)?,"dry_run":false}),
                )
                .await?;
            println!(
                "{}\nInstalled DISABLED. Grant the actor the type {} in config.json, then: plugins enable {}",
                summary(&result),
                result["type"].as_str().unwrap_or("plugin.ID"),
                result["id"].as_str().unwrap_or("ID")
            );
        }
        Plugins::Enable { id } => {
            let result = client.invoke("plugin_enable", json!({"id":id})).await?;
            println!("{}", summary(&result));
        }
        Plugins::Disable { id } => {
            let result = client.invoke("plugin_disable", json!({"id":id})).await?;
            println!("{}", summary(&result));
        }
        Plugins::Uninstall { id } => {
            let result = client.invoke("plugin_uninstall", json!({"id":id})).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Plugins::Run { id, command } => {
            let result = client
                .invoke("plugin_run", json!({"id":id,"command":command}))
                .await?;
            let status = result["status"].as_u64().unwrap_or(1).min(255) as u8;
            if result["denied"] == true {
                eprintln!(
                    "{}",
                    result["message"].as_str().unwrap_or("plugin is disabled")
                );
            } else {
                print!("{}", result["stdout"].as_str().unwrap_or(""));
                eprint!("{}", result["stderr"].as_str().unwrap_or(""));
            }
            if status != 0 {
                return Err(crate::client::ExitStatus(status).into());
            }
        }
    }
    Ok(())
}

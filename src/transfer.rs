//! Backups started through the service (online) and a listing of existing archives.
//! Restore stays an offline CLI operation: it is destructive and needs the service
//! stopped, so the service only prints the exact instruction.
use crate::core::{Core, Denied, Invalid};
use anyhow::Result;
use chrono::Utc;
use serde_json::{Value, json};
use std::sync::Arc;

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Core {
    /// Backup archives under the configured backups directory, newest first, with the
    /// state of the latest backup started through this service.
    pub fn backups(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let dir = &self.paths.backups;
        let mut archives = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let manifest: Option<Value> = std::fs::read(path.join("manifest.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                let state = if path.join("INCOMPLETE").exists() {
                    "incomplete"
                } else if manifest.is_some() {
                    "complete"
                } else {
                    "unknown"
                };
                let created = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                archives.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "path": path.to_string_lossy(),
                    "state": state,
                    "created_ms": created,
                    "files": manifest.as_ref().and_then(|m| m["files"].as_object()).map(|f| f.len()),
                    "dump_bytes": std::fs::metadata(path.join("database.dump")).map(|m| m.len()).ok(),
                    "postgres_major": manifest.as_ref().map(|m| m["postgres_major"].clone()),
                    "home_id": manifest.as_ref().map(|m| m["home_id"].clone()),
                    "restore": format!("agent-forge --home NEW_HOME restore {}", path.to_string_lossy()),
                }));
            }
        }
        archives.sort_by(|a, b| b["created_ms"].as_i64().cmp(&a["created_ms"].as_i64()));
        let job = self.backup_job.lock().map(|j| j.clone()).unwrap_or(None);
        Ok(json!({
            "directory": dir.to_string_lossy(),
            "archives": archives,
            "latest_job": job,
            "restore_instruction": "Restore is CLI-only and destructive: stop the service, point FORGE_DATABASE_URL (or --database-url) at an EMPTY PostgreSQL database, then run `agent-forge --home NEW_HOME restore ARCHIVE_PATH` into a NEW home directory. Never restore over a running or populated home.",
        }))
    }

    /// Start one online backup in the background. Only one runs at a time.
    pub async fn backup_start(self: &Arc<Self>, actor: &str, name: Option<&str>) -> Result<Value> {
        self.permit(actor, "transfer", None)?;
        let settings = self
            .settings
            .get()
            .ok_or(Invalid(
                "backups need the running service's resolved settings",
            ))?
            .clone();
        let name = match name {
            Some(n) => {
                crate::core::identifier(n)?;
                n.to_owned()
            }
            None => format!("backup-{}", Utc::now().format("%Y%m%dT%H%M%SZ")),
        };
        let destination = self.paths.backups.join(&name);
        crate::safety::private_mkdir(&self.paths.backups)?;
        if destination.exists() {
            return Err(Invalid("a backup with this name already exists").into());
        }
        {
            let mut job = self
                .backup_job
                .lock()
                .map_err(|_| anyhow::anyhow!("lock"))?;
            if job.as_ref().is_some_and(|j| j["state"] == "running") {
                return Err(Denied("a backup is already running").into());
            }
            *job = Some(json!({"name":name,"state":"running","started_ms":now_ms(),"actor":actor}));
        }
        self.record(actor, "backup_start", &name, "")?;
        let core = self.clone();
        let (job_name, who) = (name.clone(), actor.to_owned());
        tokio::spawn(async move {
            let result = crate::backup::backup_online(settings, destination, &core).await;
            let (state, error) = match &result {
                Ok(()) => ("complete", Value::Null),
                Err(e) => (
                    "failed",
                    json!(e.to_string().chars().take(300).collect::<String>()),
                ),
            };
            let _ = core.record(&who, &format!("backup_{state}"), &job_name, "");
            if let Ok(mut job) = core.backup_job.lock() {
                *job = Some(
                    json!({"name":job_name,"state":state,"finished_ms":now_ms(),"error":error,"actor":who}),
                );
            }
        });
        Ok(json!({"name":name,"state":"running","notes":crate::backup::outside_notes(&self.paths)}))
    }
}

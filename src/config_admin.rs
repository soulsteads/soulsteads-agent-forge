//! Reading and editing the operator configuration through the service: effective
//! settings with the source of each value, and the model and runner providers.
//!
//! Edits write `config.json` (atomically, privately) after the same validation a
//! restart applies (`core::validate_policy`, `validate_model`, `RunnerProfile::validate`).
//! The running service keeps the policy it loaded: every edit reports
//! `restart_required` and there is deliberately no restart operation. Credentials are
//! only ever environment-variable NAMES; no operation accepts or returns a value.
use crate::core::{Core, Denied, Invalid, Model, Policy};
use crate::harness::RunnerProfile;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

impl Core {
    fn read_config(&self) -> Result<Value> {
        let bytes = crate::safety::read_private(&self.paths.config_file())?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    /// Validate a whole candidate config, then replace `config.json` atomically (mode
    /// 0600, fsynced temp file renamed over it).
    fn write_config(&self, candidate: &Value) -> Result<()> {
        let policy: Policy = serde_json::from_value(candidate.clone())
            .map_err(|_| Invalid("configuration section or field is not supported"))?;
        crate::core::validate_policy(&self.home, &policy)
            .map_err(|e| crate::settings::ConfigError(e.to_string()))?;
        if crate::settings::restore_marker(&self.home).exists() {
            return Err(Invalid("restore incomplete; finish recovery first").into());
        }
        let target = self.paths.config_file();
        let temp = target.with_file_name(format!(".config.json.edit-{}", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(serde_json::to_string_pretty(candidate)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &target).context("config replace failed")?;
        std::fs::File::open(&self.home)?.sync_all()?;
        Ok(())
    }

    /// The effective resolved settings with each value's source (`flag`, `env`,
    /// `config` or `default`). Secrets are redacted by construction.
    pub fn settings_view(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let settings = self.settings.get().ok_or(Invalid(
            "effective settings are only available on a running service",
        ))?;
        let saved = self.read_config()?;
        let rows: Vec<Value> = settings
            .report(&self.policy)
            .into_iter()
            .map(|mut row| {
                let key = row["key"].as_str().unwrap_or("").to_owned();
                if let Some((section, name)) = key.split_once('.') {
                    // The value saved in config.json, which differs from the effective
                    // one while a restart is pending or a flag/env overrides it.
                    let on_disk = saved.get(section).and_then(|s| s.get(name)).cloned();
                    if key != "database.url" {
                        row["saved"] = on_disk.unwrap_or(Value::Null);
                    }
                }
                row
            })
            .collect();
        Ok(json!({
            "settings": rows,
            "editable": crate::settings::EDITABLE,
            "restart_note": "Edits are saved to config.json and take effect when the service is restarted by its operator. A flag or environment value still overrides a saved one.",
        }))
    }

    /// Set (or, with a null value, unset) one config.json-backed setting.
    pub fn settings_set(&self, actor: &str, key: &str, value: &Value) -> Result<Value> {
        self.permit(actor, "configure", None)?;
        if !crate::settings::EDITABLE.contains(&key) {
            return Err(
                Invalid("setting is not editable here (database.url is never accepted)").into(),
            );
        }
        let (section, name) = key.split_once('.').ok_or(Invalid("invalid setting key"))?;
        let _guard = self
            .config_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("lock"))?;
        let mut config = self.read_config()?;
        let object = config
            .as_object_mut()
            .ok_or(Invalid("config.json must be an object"))?;
        let entry = object.entry(section).or_insert_with(|| json!({}));
        if value.is_null() {
            if let Some(map) = entry.as_object_mut() {
                map.remove(name);
                if map.is_empty() {
                    object.remove(section);
                }
            }
        } else {
            entry
                .as_object_mut()
                .ok_or(Invalid("config section must be an object"))?
                .insert(name.to_owned(), value.clone());
        }
        self.write_config(&config)?;
        self.record(actor, "settings_set", "config.json", key)?;
        Ok(json!({"key":key,"saved":true,"restart_required":true}))
    }

    /// Saved (config.json) versus effective (running) model aliases and runner
    /// profiles, plus whether a restart is pending.
    pub async fn providers(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let saved: Policy = serde_json::from_value(self.read_config()?)?;
        let mut names: std::collections::BTreeSet<&String> = saved.models.keys().collect();
        names.extend(self.policy.models.keys());
        let model_row = |name: &String| {
            let model = saved
                .models
                .get(name)
                .or_else(|| self.policy.models.get(name));
            let status = match (saved.models.get(name), self.policy.models.get(name)) {
                (Some(a), Some(b))
                    if a.base_url == b.base_url && a.model == b.model && a.key_env == b.key_env =>
                {
                    "effective"
                }
                (Some(_), Some(_)) => "pending_change",
                (Some(_), None) => "pending_add",
                (None, _) => "pending_remove",
            };
            json!({
                "name": name,
                "base_url": model.map(|m| m.base_url.clone()),
                "model": model.map(|m| m.model.clone()),
                "key_env": model.and_then(|m| m.key_env.clone()),
                "key_env_set": model.and_then(|m| m.key_env.as_deref()).map(|n| std::env::var_os(n).is_some()),
                "status": status,
            })
        };
        let models: Vec<Value> = names.into_iter().map(model_row).collect();
        let runners_state = self.runners(actor).await?;
        let mut rnames: std::collections::BTreeSet<&String> = saved.runners.keys().collect();
        rnames.extend(self.policy.runners.keys());
        let mut runners = Vec::new();
        for name in rnames {
            let status = match (saved.runners.get(name), self.policy.runners.get(name)) {
                (Some(a), Some(b)) if serde_json::to_value(a)? == serde_json::to_value(b)? => {
                    "effective"
                }
                (Some(_), Some(_)) => "pending_change",
                (Some(_), None) => "pending_add",
                (None, _) => "pending_remove",
            };
            let mut row = saved
                .runners
                .get(name)
                .or_else(|| self.policy.runners.get(name))
                .map(|p| p.describe(name))
                .unwrap_or(Value::Null);
            row["status"] = json!(status);
            row["validation"] = match saved.runners.get(name) {
                Some(p) => match p.validate_for_paths(name, &self.paths) {
                    Ok(()) => json!("ok"),
                    Err(e) => json!(e.to_string()),
                },
                None => Value::Null,
            };
            if let Some(live) = runners_state["profiles"]
                .as_array()
                .and_then(|p| p.iter().find(|x| x["name"] == *name))
            {
                row["available"] = live["available"].clone();
                row["version"] = live["version"].clone();
            }
            runners.push(row);
        }
        let pending = models
            .iter()
            .chain(runners.iter())
            .any(|r| r["status"] != "effective");
        Ok(
            json!({"models":models,"runners":runners,"restart_required":pending,"adapters":crate::harness::matrix().map(|a|a.name).collect::<Vec<_>>()}),
        )
    }

    /// Add or replace a model alias or runner profile in config.json after full
    /// validation. `definition` carries only names of environment variables.
    pub fn provider_set(
        &self,
        actor: &str,
        kind: &str,
        name: &str,
        definition: &Value,
    ) -> Result<Value> {
        self.permit(actor, "configure", None)?;
        let section = match kind {
            "model" => "models",
            "runner" => "runners",
            _ => return Err(Invalid("kind must be model or runner").into()),
        };
        crate::core::identifier(name)?;
        if kind == "model" {
            let model: Model = serde_json::from_value(definition.clone()).map_err(|_| {
                Invalid("model fields: base_url, model, key_env (an environment variable name)")
            })?;
            crate::core::validate_model(name, &model)
                .map_err(|e| crate::settings::ConfigError(format!("model {name} refused: {e}")))?;
        } else {
            let runner: RunnerProfile = serde_json::from_value(definition.clone())
                .map_err(|_| Invalid("runner profile has an unsupported or missing field"))?;
            runner.validate_for_paths(name, &self.paths).map_err(|e| {
                crate::settings::ConfigError(format!("runner profile {name} refused: {e}"))
            })?;
            let literal = runner.literal_secret_env();
            if !literal.is_empty() {
                return Err(crate::settings::ConfigError(format!(
                    "runner profile {name} refused: env {} holds what looks like a literal secret. Secrets are never saved in configuration: name the variable in env_passthrough (the harness inherits it from the Forge process) instead",
                    literal.join(", ")
                ))
                .into());
            }
        }
        let _guard = self
            .config_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("lock"))?;
        let mut config = self.read_config()?;
        config
            .as_object_mut()
            .ok_or(Invalid("config.json must be an object"))?
            .entry(section)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or(Invalid("config section must be an object"))?
            .insert(name.to_owned(), definition.clone());
        self.write_config(&config)?;
        self.record(actor, &format!("provider_set_{kind}"), name, "")?;
        Ok(json!({"kind":kind,"name":name,"saved":true,"restart_required":true}))
    }

    /// Remove a model alias or runner profile unless saved work still names it.
    pub fn provider_remove(&self, actor: &str, kind: &str, name: &str) -> Result<Value> {
        self.permit(actor, "configure", None)?;
        let (section, field) = match kind {
            "model" => ("models", "model"),
            "runner" => ("runners", "profile"),
            _ => return Err(Invalid("kind must be model or runner").into()),
        };
        let needle = format!("\"{field}\":{}", serde_json::to_string(name)?);
        let referenced = self.store.write(|c| {
            let mut q = c.prepare("SELECT id,definition FROM forge_work")?;
            let rows = q
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .filter(|(_, d)| d.contains(&needle))
                .map(|(id, _)| id)
                .collect::<Vec<_>>())
        })?;
        if !referenced.is_empty() {
            return Err(Invalid("refused: registered work still references this provider").into());
        }
        let _guard = self
            .config_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("lock"))?;
        let mut config = self.read_config()?;
        let removed = config
            .get_mut(section)
            .and_then(Value::as_object_mut)
            .and_then(|m| m.remove(name));
        if removed.is_none() {
            return Err(Denied("provider is not in the saved configuration").into());
        }
        self.write_config(&config)?;
        self.record(actor, &format!("provider_remove_{kind}"), name, "")?;
        Ok(json!({"kind":kind,"name":name,"removed":true,"restart_required":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{self, Core};
    use crate::dbos::admission::AdmissionStore;
    use crate::settings::{Overrides, Paths, Settings};
    use std::{collections::BTreeMap, sync::Arc};

    fn core() -> (tempfile::TempDir, Arc<Core>) {
        let home = tempfile::tempdir().unwrap();
        core::init(home.path()).unwrap();
        let mut policy = core::load_policy(home.path()).unwrap();
        policy.actors.insert(
            "viewer".into(),
            core::Actor {
                token: "v".repeat(32),
                capabilities: vec!["read".into()],
                types: vec![],
            },
        );
        let overrides = Overrides {
            database_url: Some(format!(
                "{}://{}:{}@127.0.0.1:1/x",
                "postgresql", "secret-user", "secret-pass"
            )),
            ..Default::default()
        };
        let settings = Settings::resolve(home.path().to_path_buf(), &policy, &overrides, &|n| {
            (n == "FORGE_PORT").then(|| "8123".to_owned())
        })
        .unwrap();
        let config = settings.runtime_config(&policy).unwrap();
        let store = Arc::new(
            AdmissionStore::initialize(&Paths::defaults(home.path()), &policy.home_id).unwrap(),
        );
        let core = Core::new(
            Paths::defaults(home.path()),
            policy,
            config,
            store,
            BTreeMap::new(),
        )
        .unwrap();
        core.settings.set(settings).unwrap();
        (home, core)
    }

    #[test]
    fn effective_settings_show_sources_and_never_a_secret() {
        let (_home, core) = core();
        let view = core.settings_view("operator").unwrap();
        let text = view.to_string();
        assert!(!text.contains("secret-pass") && !text.contains("secret-user"));
        let row = |k: &str| {
            view["settings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["key"] == k)
                .unwrap()
                .clone()
        };
        assert_eq!(
            (
                row("database.url")["value"].as_str(),
                row("database.url")["source"].as_str()
            ),
            (Some("[redacted]"), Some("flag"))
        );
        assert_eq!(row("database.url")["secret"], true);
        assert_eq!(
            (
                row("server.port")["value"].as_u64(),
                row("server.port")["source"].as_str()
            ),
            (Some(8123), Some("env"))
        );
        assert_eq!(row("server.host")["source"], "default");
        assert_eq!(row("dbos.work_concurrency")["source"], "default");
        assert_eq!(row("paths.backups")["editable"], true);
    }

    #[test]
    fn settings_edits_are_validated_audited_private_and_need_a_restart() {
        let (home, core) = core();
        // Capability and key whitelist.
        assert!(
            core.settings_set("viewer", "dbos.work_concurrency", &json!(3))
                .is_err()
        );
        assert!(
            core.settings_set(
                "operator",
                "database.url",
                &json!(format!("{}://{}:{}@h/d", "postgresql", "u", "p"))
            )
            .is_err()
        );
        assert!(
            core.settings_set("operator", "server.nope", &json!(1))
                .is_err()
        );
        // Validation is the load-time validation: the file is untouched on refusal.
        let before = std::fs::read(home.path().join("config.json")).unwrap();
        assert!(
            core.settings_set("operator", "database.max_connections", &json!(1))
                .is_err()
        );
        assert!(
            core.settings_set("operator", "dbos.schema_prefix", &json!("Bad Name"))
                .is_err()
        );
        assert!(
            core.settings_set("operator", "server.host", &json!("not-an-ip"))
                .is_err()
        );
        assert_eq!(
            std::fs::read(home.path().join("config.json")).unwrap(),
            before
        );
        let saved = core
            .settings_set("operator", "dbos.work_concurrency", &json!(3))
            .unwrap();
        assert_eq!(saved["restart_required"], true);
        let on_disk: Value =
            serde_json::from_slice(&std::fs::read(home.path().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk["dbos"]["work_concurrency"], 3);
        assert_eq!(
            on_disk["actors"]["operator"]["capabilities"]
                .as_array()
                .unwrap()
                .len(),
            9
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.path().join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // The effective (running) value is unchanged until restart; the saved one differs.
        let view = core.settings_view("operator").unwrap();
        let row = view["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == "dbos.work_concurrency")
            .unwrap();
        assert_eq!(
            (row["value"].as_i64(), row["saved"].as_i64()),
            (Some(2), Some(3))
        );
        // Unsetting restores the default.
        core.settings_set("operator", "dbos.work_concurrency", &Value::Null)
            .unwrap();
        let on_disk: Value =
            serde_json::from_slice(&std::fs::read(home.path().join("config.json")).unwrap())
                .unwrap();
        assert!(on_disk.get("dbos").is_none());
        let audited: i64 = core.store.write(|c| Ok(c.query_row("SELECT count(*) FROM forge_audit WHERE action='settings_set' AND actor='operator'", [], |r| r.get(0))?)).unwrap();
        assert_eq!(audited, 2);
    }

    #[tokio::test]
    async fn providers_validate_like_load_store_only_env_names_and_track_restart() {
        let (_home, core) = core();
        let good = json!({"base_url":"http://127.0.0.1:8000/v1","model":"m","key_env":"MODEL_KEY"});
        for bad in [
            json!({"base_url":"ftp://x/v1","model":"m"}),
            json!({"base_url":format!("{}://{}:{}@x/v1", "http", "u", "p"),"model":"m"}),
            json!({"base_url":"http://x/v1","model":""}),
            json!({"base_url":"https://x/v1","model":"m","key_env":format!("sk-{}", "synthetic".repeat(4))}),
            json!({"base_url":"http://remote.example/v1","model":"m","key_env":"K"}),
            json!({"base_url":"https://x/v1","model":"m","api_key":"literal"}),
        ] {
            assert!(
                core.provider_set("operator", "model", "m1", &bad).is_err(),
                "{bad}"
            );
        }
        assert!(core.provider_set("viewer", "model", "m1", &good).is_err());
        assert!(
            core.provider_set("operator", "model", "bad name", &good)
                .is_err()
        );
        let saved = core.provider_set("operator", "model", "m1", &good).unwrap();
        assert_eq!(saved["restart_required"], true);
        let runner = json!({"adapter":"acp","executable":"/usr/bin/true","args":["acp"]});
        // The same validation as profile load: this profile is accepted; a widening one is not.
        core.provider_set("operator", "runner", "r1", &runner)
            .unwrap();
        for bad in [
            json!({"adapter":"nope","executable":"/usr/bin/true"}),
            json!({"adapter":"acp","executable":"relative","args":["acp"]}),
            json!({"adapter":"acp","executable":"/usr/bin/true"}),
            json!({"adapter":"acp","executable":"/usr/bin/true","args":["acp"],"env":{"API_TOKEN":"literal-secret-value"}}),
            json!({"adapter":"acp","executable":"/usr/bin/true","args":["acp"],"timeout_seconds":0}),
        ] {
            assert!(
                core.provider_set("operator", "runner", "r2", &bad).is_err(),
                "{bad}"
            );
        }
        let view = core.providers("operator").await.unwrap();
        assert_eq!(view["restart_required"], true);
        let model = &view["models"][0];
        assert_eq!(
            (
                model["name"].as_str(),
                model["status"].as_str(),
                model["key_env"].as_str()
            ),
            (Some("m1"), Some("pending_add"), Some("MODEL_KEY"))
        );
        assert_eq!(model["key_env_set"], false);
        assert!(!view.to_string().contains("literal"));
        let runner = &view["runners"][0];
        assert_eq!(
            (
                runner["name"].as_str(),
                runner["status"].as_str(),
                runner["validation"].as_str()
            ),
            (Some("r1"), Some("pending_add"), Some("ok"))
        );
    }

    #[test]
    fn saved_providers_are_removed_unless_work_references_them() {
        let (home, core) = core();
        let good = json!({"base_url":"http://127.0.0.1:8000/v1","model":"m"});
        core.provider_set("operator", "model", "m1", &good).unwrap();
        core.register(
            "operator",
            "uses-m1",
            json!({"type":"model","input":{"model":"m1","prompt":"hi"}}),
        )
        .ok();
        // The running policy does not know m1 yet, so registration is refused; record a
        // reference directly to prove the guard reads saved work definitions.
        core.store
            .write(|c| {
                c.execute("INSERT INTO forge_work VALUES('ref','operator','{\"type\":\"model\",\"input\":{\"model\":\"m1\",\"prompt\":\"x\"}}',1,0)", [])?;
                Ok(())
            })
            .unwrap();
        assert!(core.provider_remove("operator", "model", "m1").is_err());
        core.store
            .write(|c| {
                c.execute("DELETE FROM forge_work WHERE id='ref'", [])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            core.provider_remove("operator", "model", "m1").unwrap()["removed"],
            true
        );
        assert!(core.provider_remove("operator", "model", "m1").is_err());
        let on_disk: Value =
            serde_json::from_slice(&std::fs::read(home.path().join("config.json")).unwrap())
                .unwrap();
        assert!(on_disk["models"].as_object().unwrap().is_empty());
    }
}

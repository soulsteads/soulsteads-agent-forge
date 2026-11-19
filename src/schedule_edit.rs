//! In-place schedule edits (cron, zone, params, enabled) and tags.
//!
//! Admission stays the authority. An edit re-runs the creation checks, retains the
//! previous revision (history and queued occurrences keep their admitted context), updates
//! Forge's retained row and then DBOS's native schedule with `update_schedule`. DBOS keeps
//! `last_fired_at` and the deterministic per-tick admission key, so an edit can neither
//! replay a tick that already fired nor fire one twice. The work a schedule runs is part of
//! its identity and is not editable: use a new schedule name for different work.
use crate::core::{Core, Denied, Invalid, ScheduleContext, schedule_timing};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

const MAX_TAGS: usize = 16;

pub(crate) type ScheduleLocks = Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>;

const PENDING_EDIT: &str = "schedule has a pending edit; pause it or resubmit the recorded timing and params before another mutation";

struct PendingScheduleEdit {
    context: ScheduleContext,
    cron: String,
    tz: String,
    revision: i64,
}

/// Adds the columns and the revision table older stores lack. Idempotent.
pub(crate) fn migrate(c: &Connection) -> Result<(), crate::dbos::admission::Error> {
    let have: Vec<String> = c
        .prepare("SELECT name FROM pragma_table_info('forge_schedules')")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    for (column, ddl) in [
        ("tags", "tags TEXT NOT NULL DEFAULT '[]'"),
        ("rev", "rev INTEGER NOT NULL DEFAULT 0"),
        ("pending", "pending INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !have.iter().any(|h| h == column) {
            c.execute(&format!("ALTER TABLE forge_schedules ADD COLUMN {ddl}"), [])?;
        }
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS forge_schedule_revisions(name TEXT NOT NULL,rev INTEGER NOT NULL,context TEXT NOT NULL,cron TEXT NOT NULL,tz TEXT NOT NULL,actor TEXT NOT NULL,created REAL NOT NULL,PRIMARY KEY(name,rev));",
    )?;
    Ok(())
}

/// What an edit may change; `None` leaves a field as it is.
#[derive(Default, Debug, Clone)]
pub struct ScheduleEdit {
    pub cron: Option<String>,
    pub tz: Option<String>,
    pub enabled: Option<bool>,
    /// `Some(None)` clears the params; `Some(Some(v))` replaces them.
    pub params: Option<Option<Value>>,
    pub tags: Option<Vec<String>>,
}

/// Validated, sorted, de-duplicated tags.
pub fn normalize_tags(tags: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let t = tag.trim().to_lowercase();
        let ok = !t.is_empty()
            && t.len() <= 40
            && t.as_bytes()[0].is_ascii_alphanumeric()
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':'));
        if !ok {
            return Err(Invalid(
                "tags are 1-40 characters: letters, digits, '_', '.', '-' or ':', starting with a letter or digit",
            )
            .into());
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.len() > MAX_TAGS {
        return Err(Invalid("at most 16 tags per schedule").into());
    }
    out.sort();
    Ok(out)
}

impl Core {
    /// Serialize mutations of one canonical native name across both stores.
    /// Idle entries don't retain a lock or accumulate with historical names.
    pub(crate) async fn lock_schedule(
        &self,
        name: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let lock = {
            let mut locks = self
                .schedule_locks
                .lock()
                .map_err(|_| anyhow::anyhow!("schedule lock registry unavailable"))?;
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(name).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(name.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        Ok(lock.lock_owned().await)
    }

    pub(crate) fn require_schedule_settled(&self, name: &str) -> Result<()> {
        let pending = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT pending FROM forge_schedules WHERE name=?1",
                [name],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
        })?;
        if pending {
            return Err(Invalid(PENDING_EDIT).into());
        }
        Ok(())
    }

    fn finish_schedule_edit(&self, name: &str, rev: i64) -> Result<()> {
        let changed = self.store.write(|c| {
            Ok(c.execute(
                "UPDATE forge_schedules SET pending=0 WHERE name=?1 AND rev=?2 AND pending=1",
                params![name, rev],
            )?)
        })?;
        if changed != 1 {
            return Err(Invalid("pending schedule revision changed before confirmation").into());
        }
        Ok(())
    }

    /// The retained (authorized) definitions a native schedule may currently equal:
    /// the row, plus its previous revision while an edit is still being applied.
    pub(crate) fn schedule_expectations(
        &self,
        name: &str,
    ) -> Result<Vec<(String, String, String)>> {
        Ok(self.store.write(|c| {
            let row = c
                .query_row(
                    "SELECT context,cron,tz,pending,rev FROM forge_schedules WHERE name=?1",
                    [name],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, bool>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((context, cron, tz, pending, rev)) = row else {
                return Ok(Vec::new());
            };
            let mut out = vec![(context, cron, tz)];
            if pending
                && let Some(prev) = c
                    .query_row(
                        "SELECT context,cron,tz FROM forge_schedule_revisions WHERE name=?1 AND rev=?2",
                        params![name, rev],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?
            {
                out.push(prev);
            }
            Ok(out)
        })?)
    }

    /// Every previous revision's context (queued occurrences keep their admitted one).
    pub(crate) fn schedule_revision_contexts(&self, name: &str) -> Result<Vec<String>> {
        Ok(self.store.write(|c| {
            let mut q = c.prepare("SELECT context FROM forge_schedule_revisions WHERE name=?1")?;
            Ok(q.query_map([name], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?)
    }

    pub(crate) fn schedule_tags(&self, name: &str) -> Result<Vec<String>> {
        let raw: Option<String> = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT tags FROM forge_schedules WHERE name=?1",
                [name],
                |r| r.get(0),
            )
            .optional()?)
        })?;
        Ok(raw
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default())
    }

    pub(crate) fn schedule_revision(&self, name: &str) -> Result<i64> {
        Ok(self.store.write(|c| {
            Ok(c.query_row(
                "SELECT rev FROM forge_schedules WHERE name=?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0))
        })?)
    }

    /// Set tags on a schedule being created (before its first native tick).
    pub(crate) fn set_schedule_tags(&self, name: &str, tags: &[String]) -> Result<()> {
        let tags = serde_json::to_string(&normalize_tags(tags)?)?;
        self.store.write(|c| {
            c.execute(
                "UPDATE forge_schedules SET tags=?2 WHERE name=?1",
                params![name, tags],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    fn pending_schedule_edit(&self, name: &str) -> Result<Option<PendingScheduleEdit>> {
        let row = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT context,cron,tz,rev FROM forge_schedules WHERE name=?1 AND pending=1",
                [name],
                |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?)
        })?;
        row.map(|(context, cron, tz, revision)| {
            Ok(PendingScheduleEdit {
                context: serde_json::from_str(&context)?,
                cron,
                tz,
                revision,
            })
        })
        .transpose()
    }

    /// The caller holds the name lock and has validated the actual native row.
    async fn reconcile_schedule_edit(
        &self,
        native: &dbos::Schedule,
        pending: PendingScheduleEdit,
    ) -> Result<()> {
        let name = native.schedule_name();
        let actual = native.decode_context::<ScheduleContext>()?;
        if pending.context.name != name
            || pending.context.home_id != actual.home_id
            || pending.context.actor != actual.actor
            || pending.context.work_id != actual.work_id
        {
            return Err(Invalid("pending schedule edit changed immutable scope").into());
        }
        self.validate_scheduled_context(&pending.context)?;
        if actual != pending.context
            || native.schedule() != pending.cron
            || native.cron_timezone() != Some(pending.tz.as_str())
        {
            self.apply_native_edit(
                name,
                &pending.cron,
                &pending.tz,
                serde_json::to_value(&pending.context)?,
            )
            .await?;
        }
        self.finish_schedule_edit(name, pending.revision)
    }

    /// Reconcile authorized pending edits before scheduler launch. A failure
    /// for one name retains its revision and cannot hide the other outcomes.
    pub(crate) async fn heal_schedule_edits(&self) -> Result<Vec<String>> {
        let names: Vec<String> = self.store.write(|c| {
            let mut q =
                c.prepare("SELECT name FROM forge_schedules WHERE pending=1 ORDER BY name")?;
            Ok(q.query_map([], |r| r.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let mut unresolved = Vec::new();
        for name in names {
            let attempt = async {
                let _edit = self.lock_schedule(&name).await?;
                let Some(pending) = self.pending_schedule_edit(&name)? else {
                    return Ok::<_, anyhow::Error>(false);
                };
                let native = self
                    .native()?
                    .get_schedule(&name)
                    .await?
                    .ok_or(Invalid("pending edit has no native schedule"))?;
                self.validate_schedule(&native)?;
                self.reconcile_schedule_edit(&native, pending).await?;
                Ok(true)
            }
            .await;
            match attempt {
                Ok(true) => {
                    if self
                        .record("runtime", "schedule_edit_healed", &name, "")
                        .is_err()
                    {
                        eprintln!("could not audit healed schedule edit {name:?}");
                    }
                }
                Ok(false) => {}
                Err(_) => unresolved.push(name),
            }
        }
        Ok(unresolved)
    }

    async fn apply_native_edit(
        &self,
        name: &str,
        cron: &str,
        tz: &str,
        context: Value,
    ) -> Result<()> {
        let change = dbos::ScheduleChange {
            schedule: dbos::Change::Set(cron),
            context: dbos::Change::Set(context),
            cron_timezone: dbos::Change::Set(Some(tz)),
            ..Default::default()
        };
        self.native()?.update_schedule(name, &change).await?;
        Ok(())
    }

    /// Edit an owned schedule in place. Idempotent: an edit that changes nothing is a
    /// no-op. Returns the schedule's full view with a `changed` list.
    pub async fn schedule_update(
        &self,
        actor: &str,
        name: &str,
        edit: ScheduleEdit,
    ) -> Result<Value> {
        self.permit(actor, "schedule", None)?;
        if edit.cron.is_none()
            && edit.tz.is_none()
            && edit.enabled.is_none()
            && edit.params.is_none()
            && edit.tags.is_none()
        {
            return Err(
                Invalid("nothing to change: give cron, tz, params, enabled or tags").into(),
            );
        }
        let _edit = self.lock_schedule(name).await?;
        let native = self
            .native()?
            .get_schedule(name)
            .await?
            .ok_or(Denied("schedule missing or outside scope"))?;
        self.validate_schedule(&native)?;
        let old_ctx = native.decode_context::<ScheduleContext>()?;
        if old_ctx.actor != actor {
            return Err(Denied("schedule missing or outside scope").into());
        }
        let plugin_owned = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM forge_plugin_resources WHERE work_id=?1",
                [&old_ctx.work_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })?;
        if plugin_owned {
            return Err(Invalid("plugin-owned schedules are managed by their plugin").into());
        }
        if let Some(pending) = self.pending_schedule_edit(name)? {
            // An explicit timing resubmission may settle only the exact existing
            // revision. Defaults come from the observed native definition.
            let requested_tz = edit.tz.as_deref().or(native.cron_timezone());
            let requested_params = edit.params.as_ref().unwrap_or(&old_ctx.params);
            if edit.tags.is_some()
                || edit.enabled.is_some()
                || requested_tz != Some(pending.tz.as_str())
                || requested_params != &pending.context.params
            {
                return Err(Invalid(PENDING_EDIT).into());
            }
            let (cron, _) = schedule_timing(
                edit.cron.as_deref().unwrap_or(native.schedule()),
                &pending.tz,
            )?;
            if cron != pending.cron {
                return Err(Invalid(PENDING_EDIT).into());
            }
            self.reconcile_schedule_edit(&native, pending).await?;
            self.record(actor, "schedule_edit_healed", name, "")?;
            let mut view = self.schedule_show(actor, name, Some(5)).await?;
            view["changed"] = json!(["pending revision"]);
            return Ok(view);
        }
        let (old_cron, old_tz) = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT cron,tz FROM forge_schedules WHERE name=?1",
                [name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?)
        })?;
        let new_tz = edit.tz.clone().unwrap_or_else(|| old_tz.clone());
        let new_cron = match &edit.cron {
            Some(c) => c.clone(),
            None => old_cron.clone(),
        };
        let (cron, _) = schedule_timing(&new_cron, &new_tz)?;
        let mut ctx = old_ctx.clone();
        if let Some(params) = &edit.params {
            if let Some(value) = params {
                let definition = self.admitted(actor, &ctx.work_id)?;
                self.validate_definition(actor, &definition, "execute")?;
                if !crate::flow_core::accepts_params(&definition)
                    && (!value.is_object() || value.as_object().is_some_and(|o| !o.is_empty()))
                {
                    return Err(Invalid(
                        "scheduled params require agent, workflow or harness work",
                    )
                    .into());
                }
                self.params_reference(actor, value)?;
            }
            ctx.params = params.clone();
        }
        let mut changed: Vec<&str> = Vec::new();
        if cron != old_cron {
            changed.push("cron");
        }
        if new_tz != old_tz {
            changed.push("tz");
        }
        if ctx != old_ctx {
            changed.push("params");
        }
        if changed.is_empty()
            && edit.tags.is_none()
            && edit
                .enabled
                .is_none_or(|e| e == (native.status().as_str() == "ACTIVE"))
        {
            let mut view = self.schedule_show(actor, name, Some(5)).await?;
            view["changed"] = json!([]);
            return Ok(view);
        }
        if !changed.is_empty() {
            let raw = serde_json::to_string(&ctx)?;
            let old_raw = serde_json::to_string(&old_ctx)?;
            let native_context = serde_json::to_value(&ctx)?;
            let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.;
            let rev = self.store.write(|c| {
                let rev: i64 = c.query_row("SELECT rev FROM forge_schedules WHERE name=?1 AND pending=0", [name], |r| r.get(0))?;
                let next = rev.checked_add(1).ok_or(crate::dbos::admission::Error::Invalid("schedule revision exhausted"))?;
                c.execute(
                    "INSERT INTO forge_schedule_revisions(name,rev,context,cron,tz,actor,created) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![name, next, old_raw, old_cron, old_tz, actor, now],
                )?;
                c.execute(
                    "UPDATE forge_schedules SET context=?2,cron=?3,tz=?4,rev=?5,pending=1 WHERE name=?1",
                    params![name, raw, cron, new_tz, next],
                )?;
                Ok(next)
            })?;
            // An error or a dropped future may leave the native commit outcome
            // unknown. Keep both authorized definitions and block further
            // mutations until startup reconciles this exact revision.
            self.apply_native_edit(name, &cron, &new_tz, native_context)
                .await?;
            self.finish_schedule_edit(name, rev)?;
        }
        if let Some(tags) = &edit.tags {
            let before = self.schedule_tags(name)?;
            self.set_schedule_tags(name, tags)?;
            if before != self.schedule_tags(name)? {
                changed.push("tags");
            }
        }
        if let Some(enabled) = edit.enabled {
            let active = native.status().as_str() == "ACTIVE";
            if enabled && !active {
                self.native()?.resume_schedule(name).await?;
                changed.push("enabled");
            } else if !enabled && active {
                self.native()?.pause_schedule(name).await?;
                changed.push("enabled");
            }
        }
        self.record(actor, "schedule_update", name, &changed.join(","))?;
        let mut view = self.schedule_show(actor, name, Some(5)).await?;
        view["changed"] = json!(changed);
        Ok(view)
    }

    /// Owned schedules, optionally only those carrying `tag`.
    pub async fn schedules_tagged(&self, actor: &str, tag: Option<&str>) -> Result<Value> {
        let all = self.schedules(actor).await?;
        let Some(tag) = tag else { return Ok(all) };
        identifier_tag(tag)?;
        let tag = tag.trim().to_lowercase();
        Ok(json!(
            all.as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["tags"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|x| x == &json!(tag))))
                .cloned()
                .collect::<Vec<_>>()
        ))
    }
}

fn identifier_tag(tag: &str) -> Result<()> {
    normalize_tags(&[tag.to_owned()]).map(|_| ())
}

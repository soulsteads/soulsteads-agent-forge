// Operator views added on top of forge-ui.js: overview, agents, approvals, skills,
// providers, settings and transfer, plus the run-detail additions (effects, live
// harness/ACP transcript, permission decisions, logs, approvals and reconciliation).
// Every value is rendered with textContent or DOM nodes; the bearer token is only used
// through window.forge (kept in page memory, sent in an Authorization header).
const F = window.forge;
const U = F.ui;
const { h, chip, when, alertBox, guarded, go, brief, pre, newKey, registerView } = U;
const $ = (id) => document.getElementById(id);

const table = (headers, rows, attrs = {}) => h("table", attrs,
  h("thead", {}, h("tr", {}, headers.map((t) => h("th", { text: t })))),
  h("tbody", {}, rows));
const td = (...c) => h("td", {}, ...c);
const runLink = (id, label) => h("a", { href: "#/runs/" + encodeURIComponent(id), title: id, text: label || String(id).slice(0, 14) + "…" });
const ms = (v) => (typeof v === "number" ? new Date(v).toISOString().replace("T", " ").replace(/\.\d+Z$/, " UTC") : "-");
const secs = (v) => (typeof v === "number" ? (v / 1000).toFixed(1) + "s" : "-");
const okChip = (ok, yes = "ok", no = "unavailable") => chip(ok ? yes.toUpperCase() : no.toUpperCase());
function refreshButton(fn) { return h("button", { type: "button", class: "refresh", text: "Refresh", onclick: fn }); }

// ------------------------------------------------------------------ doctor
// Read-only diagnosis, run on demand (it probes runners and validates every definition).
const DOCTOR_CHIP = { ok: "SUCCESS", warn: "PENDING", fail: "ERROR", skip: "CANCELLED" };
function doctorCard() {
  const out = h("div", { id: "ov-doctor-result" }, h("p", { class: "empty", text: "Not run yet. Doctor changes nothing." }));
  const run = h("button", { type: "button", id: "ov-doctor-run", class: "primary", text: "Run doctor" });
  run.onclick = () => guarded($("overview-alert"), async () => {
    run.disabled = true; run.textContent = "Running…";
    try {
      const r = await F.call("doctor");
      out.replaceChildren(
        h("p", { id: "ov-doctor-status" }, chip(DOCTOR_CHIP[r.status] === "SUCCESS" ? "SUCCESS" : DOCTOR_CHIP[r.status]), " ",
          r.status.toUpperCase() + ": " + r.summary.ok + " ok, " + r.summary.warn + " warn, " + r.summary.fail + " fail, " + r.summary.skip + " skipped"),
        table(["Check", "Status", "Detail"], r.checks.map((c) => h("tr", { "data-check": c.id, "data-status": c.status },
          td(h("code", { text: c.id })), td(chip(DOCTOR_CHIP[c.status])), td(c.detail, c.fix ? h("pre", { class: "doctor-fix", text: c.fix }) : ""))), { id: "ov-doctor-table" }));
    } finally { run.disabled = false; run.textContent = "Run doctor"; }
  });
  return h("div", { class: "card", id: "ov-doctor" }, h("div", { class: "row-actions" }, h("h3", { text: "Doctor" }), run),
    h("p", { class: "hint", text: "Checks config, database, DBOS schema, runners, plugin integrity, every work definition and schedule, lost runs and missing authority for newer operations. Read-only." }), out);
}

// ------------------------------------------------------------------ overview
function failureDetail(run) {
  return run.failure?.reason
    ? (run.failure.step ? "step " + run.failure.step + ": " : "") + brief(run.failure.reason, 140)
    : brief(run.diagnostic || "see run", 140);
}

function attentionCard(approvals, recoveries, incompleteRuns) {
  const pending = Array.isArray(approvals?.approvals) ? approvals.approvals : null;
  const actionable = pending?.filter((a) => a?.state === "pending" && a.actionable === true).length;
  const incompleteApprovals = pending?.some((a) => a?.state !== "pending" || typeof a.actionable !== "boolean");
  const approvalStatus = actionable === undefined ? "Pending approvals are unavailable."
    : actionable > 0 ? actionable + (actionable === 1 ? " actionable approval shown." : " actionable approvals shown.")
    : incompleteApprovals ? "" : "No actionable pending approvals in the returned list.";
  const runStatus = recoveries === null ? "Recent run information is unavailable."
    : incompleteRuns ? "Recent run information is incomplete."
    : recoveries.length ? "Check the evidence in these recent runs before recording a decision."
    : "No runs marked for reconciliation among recent runs shown.";
  return h("section", { class: "card", "aria-labelledby": "ov-attention-heading" },
    h("h3", { id: "ov-attention-heading", text: "Needs attention" }),
    h("ol", {},
      h("li", {}, h("a", { href: "#/approvals?state=pending", text: "Review pending approvals" }),
        h("p", { text: [
          approvalStatus,
          incompleteApprovals ? "Pending approval information is incomplete." : "",
          actionable > 0 ? "Record each decision with a note." : "",
        ].filter(Boolean).join(" ") })),
      h("li", {}, h("strong", { text: "Check runs that need reconciliation" }), h("p", { text: runStatus }),
        recoveries?.length ? h("ul", {}, recoveries.map((run) =>
          h("li", {}, runLink(run.run_id, run.work_id || run.run_id), " — ", failureDetail(run)))) : "")));
}

let overviewRead = 0;
async function overview({ fromRoute = true, silent = false } = {}) {
  const onOverview = () => U.parseHash().view === "overview";
  if (!onOverview()) return;
  const body = $("overview-body");
  const read = ++overviewRead, generation = U.routeGeneration();
  const isCurrent = () => generation === U.routeGeneration() && read === overviewRead && onOverview();
  let o, approvals;
  try {
    [o, approvals] = await Promise.all([
      F.call("overview"),
      F.call("approvals", { state: "pending", limit: 100 }).catch(() => null),
    ]);
  } catch (error) {
    if (isCurrent() && !silent) {
      const message = error.message || String(error);
      alertBox($("overview-alert"), message);
      if (fromRoute) F.status("Failed / state may be stale: " + message);
    }
    return;
  }
  if (!isCurrent()) return;
  const health = o.health;
  const recent = Array.isArray(o.failures)
    ? o.failures.filter((f) => typeof f?.run_id === "string" && f.run_id && typeof f.status === "string")
    : null;
  const incompleteRuns = recent !== null && recent.length !== o.failures.length;
  const recoveries = recent?.filter((f) => f.status === "RECOVERY_REQUIRED") ?? null;
  const ordinaryFailures = recent?.filter((f) => f.status !== "RECOVERY_REQUIRED") ?? [];
  const failures = ordinaryFailures.length
    ? table(["Status", "Work", "Run", "Why"], ordinaryFailures.map((f) => h("tr", { "data-run": f.run_id },
      td(chip(f.status)), td(f.work_id), td(runLink(f.run_id)), td(failureDetail(f)))), { id: "ov-failures" })
    : h("p", { class: "empty", id: "ov-failures-empty", text: recent === null ? "Recent failures are unavailable."
      : incompleteRuns ? "Recent failure information is incomplete." : "No other recent failures shown." });
  const upcoming = o.upcoming.length
    ? table(["Next due", "Schedule", "Work", "Cron · zone"], o.upcoming.map((u) => h("tr", {},
      td(U.iso(u.next_run)), td(h("a", { href: "#/schedules/" + encodeURIComponent(u.schedule_name), text: u.schedule_name })),
      td(u.work_id), td(h("code", { text: u.schedule }), " ", h("small", { text: u.cron_timezone || "UTC" })))), { id: "ov-upcoming" })
    : h("p", { class: "empty", id: "ov-upcoming-empty", text: "No active schedules. Create one in Schedules." });
  body.replaceChildren(
    h("div", { class: "row-actions" }, h("h2", { text: "Overview" }), refreshButton(() => guarded($("overview-alert"), () => overview({ fromRoute: false })))),
    attentionCard(approvals, recoveries, incompleteRuns),
    h("div", { class: "card", id: "ov-health" },
      h("h3", { text: "Health" }),
      table(["Component", "State"], [
        h("tr", {}, td("Service"), td(okChip(health.service === "ok"))),
        h("tr", {}, td("DBOS engine"), td(health.engine, " ", okChip(true, "running"))),
        h("tr", {}, td("Database"), td(okChip(health.database === "ok"))),
        h("tr", {}, td("Scheduler"), td(health.scheduler)),
        h("tr", {}, td("Work"), td(String(o.counts.work) + " definitions")),
        h("tr", {}, td("Approvals"), td(h("a", { href: "#/approvals", text: o.counts.pending_approvals + " pending" }))),
      ]),
      h("h4", { text: "Runners and harness availability" }),
      health.runners.length ? table(["Runner", "Adapter", "Model", "Version", "Available"], health.runners.map((r) => h("tr", { "data-runner": r.name },
        td(r.name), td(r.adapter), td(r.model || "native default"), td(r.version || "unknown"),
        td(okChip(r.available, "available", "unavailable"), r.available ? "" : " " + (r.reason || ""))))) : h("p", { class: "empty", text: "No runner profiles configured." }),
      h("h4", { text: "Plugins" }),
      health.plugins.length ? table(["Plugin", "Version", "State", "Health"], health.plugins.map((p) => h("tr", {},
        td(p.id), td(p.version), td(p.state), td(p.problem ? h("span", { class: "danger", text: p.problem }) : "ok")))) : h("p", { class: "empty", text: "No plugins installed." }),
      h("p", { class: "hint", text: "Checked " + health.time })),
    doctorCard(),
    h("div", { class: "card" }, h("h3", { text: "Upcoming work" }), upcoming),
    h("div", { class: "card" }, h("h3", { text: "Recent failures" }), failures));
  U.setPoll(() => { if (onOverview()) overview({ fromRoute: false, silent: true }).catch(() => {}); }, 8000);
}

// ------------------------------------------------------------------ agents
async function agents() {
  const body = $("agents-body"), diag = $("agents-alert");
  const a = await F.call("agents");
  const row = (r) => h("tr", { "data-run": r.run_id, "data-step": r.step },
    td(chip(r.status)), td(r.runner), td(r.adapter || "-", r.transport ? " (" + r.transport + ")" : ""), td(r.model || "-"),
    td(secs(r.duration_ms)), td(String(r.tool_events ?? "-")), td(String(r.permission_denials ?? "-")), td(String(r.permission_withheld ?? "-")),
    td(String(r.permission_unconfirmed ?? "-"), r.permission_evidence_incomplete ? h("small", { class: "hint", text: " · permission counts incomplete" }) : ""),
    td(runLink(r.run_id), " · ", h("small", { text: r.step })),
    td(r.actions.cancel ? h("button", { type: "button", class: "danger", text: "Cancel", onclick: () => guarded(diag, async () => { await F.call("cancel", { run_id: r.run_id }); await agents(); }) }) : ""));
  const head = ["Status", "Runner", "Adapter", "Model", "Duration", "Tool events", "Denied", "Withheld", "Unconfirmed", "Run · step", ""];
  const restore = [];
  const agentTable = (label, id, runs) => {
    const previous = $(id)?.parentElement;
    const region = h("div", { class: "scroll", role: "region", "aria-label": label, tabindex: "0" },
      table(head, runs.map(row), { id }));
    if (previous) {
      const focused = previous.contains(document.activeElement) ? document.activeElement : null;
      restore.push({ region, left: previous.scrollLeft, focused });
    }
    return region;
  };
  body.replaceChildren(
    h("div", { class: "row-actions" }, h("h2", { text: "Agents" }), refreshButton(() => guarded(diag, agents))),
    h("p", { text: "Agent runs are harness and ACP workloads executed by operator-approved runner profiles inside durable runs. Choose a run for its live transcript, tool events and permission decisions." }),
    h("div", { class: "card" }, h("h3", { text: "Active" }),
      a.active.length ? agentTable("Active agent runs", "agents-active", a.active) : h("p", { class: "empty", id: "agents-active-empty", text: "No agent runs are active." })),
    h("div", { class: "card" }, h("h3", { text: "Recent" }),
      a.recent.length ? agentTable("Recent agent runs", "agents-recent", a.recent) : h("p", { class: "empty", text: "No agent runs yet." })));
  for (const { region, left, focused } of restore) {
    region.scrollLeft = left;
    if (focused) {
      const identity = focused.closest("tr[data-run]")?.dataset;
      const sameStep = identity && Array.from(region.querySelectorAll("tr[data-run]")).find(row =>
        row.dataset.run === identity.run && row.dataset.step === identity.step);
      (sameStep?.querySelector(focused.localName) || region).focus({ preventScroll: true });
    }
  }
  if (a.active.length) U.setPoll(() => { if (U.parseHash().view === "agents") agents().catch(() => {}); }, 2500);
}

// ------------------------------------------------------------------ approvals
function approvalControls(a, diag, reload) {
  const note = h("input", { type: "text", class: "note", placeholder: "Decision note (required)", maxlength: 1024, "aria-label": "Decision note" });
  const decide = (method) => guarded(diag, async () => {
    if (!note.value.trim()) throw Error("A decision note is required.");
    await F.call(method, { approval_id: a.approval_id, note: note.value.trim() });
    await reload();
  });
  return h("span", { class: "approval-controls" }, note,
    h("button", { type: "button", class: "primary approve", text: "Approve", onclick: () => decide("approve") }),
    h("button", { type: "button", class: "danger reject", text: "Reject", onclick: () => decide("reject") }));
}
const approvalState = (a) => chip(a.state === "timed_out" ? "TIMED_OUT" : a.state.toUpperCase());
async function approvals(query) {
  const body = $("approvals-body"), diag = $("approvals-alert");
  const reload = () => approvals(query);
  const filter = h("select", { id: "ap-state" }, ["", "pending", "approved", "rejected", "timed_out"].map((s) => h("option", { value: s, text: s || "Any state" })));
  filter.value = query?.get("state") || "";
  filter.onchange = () => go("#/approvals" + (filter.value ? "?state=" + filter.value : ""));
  const r = await F.call("approvals", filter.value ? { state: filter.value, limit: 100 } : { limit: 100 });
  const rows = r.approvals.map((a) => h("tr", { "data-approval": a.approval_id, "data-state": a.state },
    td(approvalState(a)), td(h("strong", { text: a.title }), a.description ? h("div", { class: "hint", text: a.description }) : ""),
    td(runLink(a.run_id), " · ", h("small", { text: a.step })),
    td(ms(a.requested_ms), a.expires_ms ? h("div", { class: "hint", text: "expires " + ms(a.expires_ms) }) : ""),
    td(a.state === "pending"
      ? (a.actionable ? approvalControls(a, diag, reload) : h("span", { class: "hint", text: "The run is " + (a.run_status || "no longer waiting").toLowerCase() + "; no decision possible." }))
      : h("span", {}, a.actor + ": ", a.note || "", h("div", { class: "hint", text: ms(a.decided_ms) + (a.delivery === "pending_retry" ? " · delivery pending" : "") })))));
  body.replaceChildren(
    h("div", { class: "row-actions" }, h("h2", { text: "Approvals" }), filter, refreshButton(() => guarded(diag, reload))),
    h("p", { text: "A workflow step of type approval waits durably (across restarts) until you approve or reject it with a note. A rejection or timeout fails that step. ACP permission requests are separate: they are always answered by the deny-by-default policy and never wait for a human." }),
    r.approvals.length ? table(["State", "Request", "Run · step", "Requested", "Decision"], rows, { id: "ap-table" })
      : h("p", { class: "empty", id: "ap-empty", text: "No approvals" + (filter.value ? " in this state." : " yet. Add an approval step to a workflow.") }));
}

// ------------------------------------------------------------------ skills
let skillCatalogRead = 0;
let skillCatalogPending = false;
let skillShowRead = 0;
async function skills() {
  const body = $("skills-body"), diag = $("skills-alert");
  const read = ++skillCatalogRead, generation = U.routeGeneration();
  ++skillShowRead;
  skillCatalogPending = true;
  const isCurrent = () => generation === U.routeGeneration() && read === skillCatalogRead && U.parseHash().view === "skills";
  let r;
  try {
    r = await F.call("skills");
  } catch (error) {
    if (isCurrent()) {
      skillCatalogPending = false;
      const message = error.message || String(error);
      alertBox(diag, message);
      F.status("Failed / state may be stale: " + message);
    }
    return;
  }
  if (!isCurrent()) return;
  skillCatalogPending = false;
  const shown = h("div", { id: "sk-content" });
  const isMounted = () => U.parseHash().view === "skills" && $("sk-content") === shown;
  const rows = r.skills.map((s) => h("tr", { "data-skill": s.name },
    td(h("code", { text: s.name })), td(s.description), td(String(s.bytes) + " bytes"), td(h("code", { text: s.sha256 })),
    td(h("button", { type: "button", text: "Show", onclick: async () => {
      if (!isMounted()) return;
      const ownsPane = !skillCatalogPending;
      const showRead = ++skillShowRead, generation = U.routeGeneration();
      const isCurrentShow = () => ownsPane && generation === U.routeGeneration() && showRead === skillShowRead && isMounted();
      if (ownsPane) alertBox(diag, "");
      try {
        const full = await F.call("skill_show", { name: s.name });
        if (isCurrentShow()) shown.replaceChildren(h("h3", { text: full.name }), pre(full.content, { role: "region", "aria-label": "Skill text", tabindex: "0" }));
      } catch (error) {
        if (isCurrentShow()) alertBox(diag, error.message || String(error));
      }
    } }))));
  body.replaceChildren(h("h2", { text: "Skills" }),
    h("p", { text: "Read-only catalog of the skills compiled into this Forge build, the same text `soulsteads-forge skill` prints. Skills are distributed with releases; there is no install or edit lifecycle here." }),
    r.skills.length ? U.scrollTable("Skill catalog", table(["Skill", "Description", "Size", "sha256", ""], rows, { id: "sk-table" })) : h("p", { class: "empty", text: "No skills." }), shown);
}

// ------------------------------------------------------------------ providers
const ENDPOINT_EXAMPLE = '{"base_url":"http://127.0.0.1:8000/v1","model":"my-model","key_env":null}';
const RUNNER_EXAMPLE = JSON.stringify({ adapter: "opencode-acp", executable: "/absolute/path/to/opencode", args: ["acp"], model: "provider/model", permission_mode: "read-only", timeout_seconds: 300 }, null, 2);
const PROVIDER_STATE = {
  effective: "Effective",
  pending_add: "Saved, pending addition",
  pending_change: "Saved, pending replacement",
  pending_remove: "Effective until restart",
};
const providerState = (status) => PROVIDER_STATE[status] || status;
async function providers() {
  const body = $("providers-body"), diag = $("providers-alert");
  const reload = () => providers();
  const p = await F.call("providers");
  const edit = (kind, name, definition) => guarded(diag, async () => { await F.call("provider_set", { kind, name, definition }); await reload(); });
  const remove = (kind, name) => guarded(diag, async () => { await F.call("provider_remove", { kind, name }); await reload(); });
  const modelName = h("input", { id: "pv-model-name", required: true, size: 18, placeholder: "alias" });
  const modelUrl = h("input", { id: "pv-model-url", required: true, size: 38, placeholder: "http://127.0.0.1:8000/v1" });
  const modelId = h("input", { id: "pv-model-id", required: true, size: 20, placeholder: "model id" });
  const modelKey = h("input", { id: "pv-model-key", size: 20, placeholder: "ENV_VAR_NAME (optional)" });
  const modelResult = h("div", { id: "pv-model-result" });
  const runnerName = h("input", { id: "pv-runner-name", required: true, size: 18, placeholder: "profile name" });
  const runnerJson = h("textarea", { id: "pv-runner-json", rows: 10, text: RUNNER_EXAMPLE });
  const runnerResult = h("div", { id: "pv-runner-result" });
  const modelRows = p.models.map((m) => h("tr", { "data-model": m.name },
    td(m.name), td(m.base_url || "-"), td(m.model || "-"),
    td(m.key_env ? h("code", { text: m.key_env }) : "none", m.key_env ? " " : "", m.key_env ? chip(m.key_env_set ? "SET" : "UNSET") : ""),
    td(providerState(m.status)),
    td(h("button", { type: "button", text: "Edit", onclick: () => { modelName.value = m.name; modelUrl.value = m.base_url || ""; modelId.value = m.model || ""; modelKey.value = m.key_env || ""; modelName.scrollIntoView(); } }),
      m.status === "pending_remove" ? "" : h("button", { type: "button", class: "danger", text: "Remove", onclick: () => remove("model", m.name) }))));
  const runnerRows = p.runners.map((r) => h("tr", { "data-runner": r.name },
    td(r.name), td(r.adapter), td(r.transport || "-"), td(r.executable || "-"), td(r.model || "native default"), td(r.permission_mode),
    td(providerState(r.status)), td(r.available === undefined ? "Not loaded" : r.available ? "available" : "unavailable"),
    td(r.validation == null ? "-" : r.validation === "ok" ? "valid" : h("span", { class: "danger", text: String(r.validation) })),
    td(r.status === "pending_add" ? "" : h("button", { type: "button", class: "test", text: "Test", title: "Probe the effective runner profile (--version only; no prompt, no model)", onclick: () => guarded(diag, async () => {
      const t = await F.call("runner_test", { name: r.name });
      alertBox(diag, "Runner " + t.name + ": " + (t.available ? "available" : "UNAVAILABLE") + " (" + t.checks.map((c) => c.id + " " + (c.ok ? "ok" : "failed: " + c.detail)).join("; ") + ")" + (t.version ? " version " + t.version : ""), t.available ? "ok" : "error");
    }) }), r.status === "pending_remove" ? "" : h("button", { type: "button", text: "Remove", class: "danger", onclick: () => remove("runner", r.name) }))));
  body.replaceChildren(
    h("div", { class: "row-actions" }, h("h2", { text: "Providers and runners" }), refreshButton(() => guarded(diag, reload))),
    p.restart_required ? h("div", { class: "alert warn", id: "pv-restart", role: "status", text: "Saved changes are pending: they take effect when the service is restarted by its operator. Forge has no restart operation." }) : "",
    h("p", { text: "Rows show the saved definition, or the effective definition until its removal takes effect. Edits use the same validation as startup. Credentials are only shown as environment variable names." }),
    h("div", { class: "card" }, h("h3", { text: "Model aliases" }),
      p.models.length ? table(["Alias", "Endpoint", "Model", "Credential variable", "Configuration state", ""], modelRows, { id: "pv-models" }) : h("p", { class: "empty", text: "No model aliases." }),
      h("h4", { text: "Add or replace a model alias" }),
      h("div", { class: "row-actions" }, h("label", {}, "Alias", modelName), h("label", {}, "Endpoint", modelUrl), h("label", {}, "Model", modelId), h("label", {}, "Credential env var name", modelKey),
        h("button", { type: "button", id: "pv-model-save", class: "primary", text: "Validate and save", onclick: () => guarded(modelResult, async () => {
          const definition = { base_url: modelUrl.value.trim(), model: modelId.value.trim(), key_env: modelKey.value.trim() || null };
          const r = await F.call("provider_set", { kind: "model", name: modelName.value.trim(), definition });
          alertBox(modelResult, "Saved " + r.name + ". Restart required.", "ok");
          await reload();
        }) })), modelResult),
    h("div", { class: "card" }, h("h3", { text: "Runner profiles" }),
      p.runners.length ? table(["Profile", "Adapter", "Transport", "Executable", "Model", "Permission", "Configuration state", "Effective availability", "Saved validation", ""], runnerRows, { id: "pv-runners" }) : h("p", { class: "empty", text: "No runner profiles." }),
      h("h4", { text: "Add or replace a runner profile" }),
      h("p", { class: "hint", text: "Adapters: " + p.adapters.join(", ") + ". Pass secrets by naming variables in env_passthrough; env holds static non-secret values only." }),
      h("div", { class: "row-actions" }, h("label", {}, "Profile name", runnerName)),
      h("label", { class: "wide" }, "Profile JSON", runnerJson),
      h("div", { class: "row-actions" }, h("button", { type: "button", id: "pv-runner-save", class: "primary", text: "Validate and save", onclick: () => guarded(runnerResult, async () => {
        let definition;
        try { definition = JSON.parse(runnerJson.value); } catch { throw Error("The profile is not valid JSON."); }
        const r = await F.call("provider_set", { kind: "runner", name: runnerName.value.trim(), definition });
        alertBox(runnerResult, "Saved " + r.name + ". Restart required.", "ok");
        await reload();
      }) })), runnerResult));
}

// ------------------------------------------------------------------ settings
const parseSetting = (raw) => { const t = raw.trim(); if (!t) throw Error("Enter a value, or use Unset to restore the default."); try { return JSON.parse(t); } catch { return t; } };
async function settings() {
  const body = $("settings-body"), diag = $("settings-alert");
  const reload = () => settings();
  const v = await F.call("settings");
  const rows = v.settings.map((s) => {
    const input = h("input", { type: "text", size: 26, "aria-label": "New value for " + s.key, placeholder: "new value" });
    return h("tr", { "data-key": s.key },
      td(h("code", { text: s.key })),
      td(s.secret ? h("em", { text: s.value }) : h("code", { text: typeof s.value === "string" ? s.value : JSON.stringify(s.value) })),
      td(chip(s.source.toUpperCase())),
      td(s.saved === undefined || s.saved === null ? "-" : h("code", { text: typeof s.saved === "string" ? s.saved : JSON.stringify(s.saved) })),
      td(s.editable ? h("span", { class: "setting-edit" }, input,
        h("button", { type: "button", class: "save", text: "Save", onclick: () => guarded(diag, async () => { const r = await F.call("settings_set", { key: s.key, value: parseSetting(input.value) }); alertBox(diag, "Saved " + r.key + ". Restart required.", "ok"); await reload2(); }) }),
        h("button", { type: "button", class: "unset", text: "Unset", onclick: () => guarded(diag, async () => { await F.call("settings_set", { key: s.key, value: null }); await reload2(); }) }))
        : h("span", { class: "hint", text: s.secret ? "never editable here" : "startup only" })));
  });
  // Keep the success message visible across the reload that follows an edit.
  const reload2 = async () => { await reload(); alertBox(diag, "Saved to config.json. Restart required for the effective value to change.", "ok"); };
  body.replaceChildren(
    h("div", { class: "row-actions" }, h("h2", { text: "Settings" }), refreshButton(() => guarded(diag, reload))),
    h("div", { class: "alert info", id: "st-note", text: v.restart_note }),
    h("p", { text: "Effective values come from the startup resolver: a command-line flag overrides an environment variable, which overrides config.json, which overrides the default. Secret values are redacted. Saved shows what config.json currently holds." }),
    table(["Setting", "Effective", "Source", "Saved", "Edit"], rows, { id: "st-table" }),
    await serviceCard(diag));
}
async function serviceCard(diag) {
  const st = await F.call("service", { action: "status" });
  const out = h("div", { id: "st-service-result" });
  const envFile = h("input", { id: "st-service-env", size: 40, placeholder: "/path/to/private.env (mode 0600; defines the database variable)" });
  const preview = () => guarded(diag, async () => {
    const r = await F.call("service", { action: "preview", ...(envFile.value.trim() ? { env_file: envFile.value.trim() } : {}) });
    out.replaceChildren(h("pre", { id: "st-service-unit", text: r.text }));
  });
  const act = (action) => guarded(diag, async () => {
    const r = await F.call("service", { action, ...(envFile.value.trim() ? { env_file: envFile.value.trim() } : {}) });
    alertBox(diag, action + " " + r.unit + ": done", "ok");
    await settings();
  });
  return h("div", { class: "card", id: "st-service" }, h("h3", { text: "Service (systemd --user)" }),
    h("p", { id: "st-service-state", text: "Unit " + st.unit + ": " + (st.installed ? "installed" : "not installed") + "; manager " + (st.systemd_user_available ? "available" : "unavailable") + "; active " + st.active + ", enabled " + st.enabled + "." }),
    h("p", { class: "hint", text: "The unit is generated from the resolved settings. It never contains the database credential: an environment file reference or the private config.json supplies it." }),
    h("div", { class: "row-actions" }, h("label", {}, "Environment file", envFile),
      h("button", { type: "button", id: "st-service-preview", text: "Preview unit", onclick: preview }),
      st.installed ? h("button", { type: "button", id: "st-service-uninstall", class: "danger", text: "Uninstall", onclick: () => act("uninstall") })
        : h("button", { type: "button", id: "st-service-install", text: "Install", onclick: () => act("install") })), out);
}

// ------------------------------------------------------------------ transfer
async function transfer() {
  const body = $("transfer-body"), diag = $("transfer-alert");
  const t = await F.call("backups");
  const name = h("input", { id: "tr-name", size: 28, placeholder: "optional name (default: timestamp)" });
  const job = t.latest_job;
  const rows = t.archives.map((a) => h("tr", { "data-backup": a.name },
    td(a.name), td(chip(a.state === "complete" ? "SUCCESS" : a.state.toUpperCase())), td(ms(a.created_ms)),
    td(a.files == null ? "-" : String(a.files) + " files"), td(a.dump_bytes == null ? "-" : String(a.dump_bytes) + " bytes"),
    td(h("code", { text: a.restore }))));
  body.replaceChildren(
    h("div", { class: "row-actions" }, h("h2", { text: "Transfer" }), refreshButton(() => guarded(diag, transfer))),
    h("p", { text: "Start a backup of the running service: the database and the private home are archived into the configured backups directory. Backups are crash-consistent: work in flight restores as recoverable and unknown effects stay unresolved for reconciliation." }),
    h("div", { class: "card" }, h("h3", { text: "Back up" }),
      h("div", { class: "row-actions" }, h("label", {}, "Backup name", name),
        h("button", { type: "button", id: "tr-start", class: "primary", text: "Start backup", disabled: job && job.state === "running", onclick: () => guarded(diag, async () => { const r = await F.call("backup_start", name.value.trim() ? { name: name.value.trim() } : {}); alertBox(diag, "Backup " + r.name + " started.", "ok"); await transfer(); alertBox(diag, "Backup " + r.name + " started.", "ok"); }) })),
      job ? h("p", { id: "tr-job" }, "Latest: ", h("code", { text: job.name }), " ", chip(job.state === "complete" ? "SUCCESS" : job.state.toUpperCase()), job.error ? h("span", { class: "danger", text: " " + job.error }) : "") : h("p", { class: "empty", text: "No backup started since the service started." }),
      h("p", { class: "hint", text: "Directory: " + t.directory })),
    h("div", { class: "card" }, h("h3", { text: "Archives" }),
      t.archives.length ? table(["Name", "State", "Created", "Files", "Database dump", "Restore (CLI only)"], rows, { id: "tr-table" }) : h("p", { class: "empty", id: "tr-empty", text: "No backups yet." })),
    h("div", { class: "card" }, h("h3", { text: "Restore" }), h("p", { id: "tr-restore", text: t.restore_instruction })),
    definitionsCard());
  if (job && job.state === "running") U.setPoll(() => { if (U.parseHash().view === "transfer") transfer().catch(() => {}); }, 1500);
}

// Work and schedule definitions: export to JSON, validate/import (all-or-nothing, idempotent).
function definitionsCard() {
  // The HTTP API limits the complete JSON request body to 1 MiB.
  const limit = 1024 * 1024;
  const hint = "Validate the current JSON before importing.";
  const box = h("textarea", { id: "tr-defs", rows: 10, placeholder: "Paste a definitions document here." });
  const file = h("input", { type: "file", accept: ".json,application/json" });
  const replace = h("input", { type: "checkbox", id: "tr-defs-replace" });
  const notice = h("div", { id: "tr-defs-notice" });
  const result = h("div", { id: "tr-defs-result" });
  file.style.maxWidth = "100%";
  file.style.boxSizing = "border-box";
  notice.style.overflowWrap = result.style.overflowWrap = "anywhere";
  let version = 0, pending = null, preview = null;

  const canImport = () => !pending && preview !== null &&
    preview.text === box.value && preview.replace === replace.checked;
  const update = () => {
    const importing = pending === "import";
    box.disabled = file.disabled = replace.disabled = exportButton.disabled = importing;
    validate.disabled = pending !== null;
    apply.disabled = !canImport();
  };
  const invalidate = (operation = null) => {
    pending = operation;
    preview = null;
    result.replaceChildren();
    alertBox(notice, "");
    update();
    return ++version;
  };
  const operate = async (operation, message, action) => {
    const requested = invalidate(operation);
    alertBox(notice, message, "info");
    const current = () => box.isConnected && version === requested;
    try { await action(current); }
    catch (e) { if (current()) alertBox(notice, e.message || String(e)); }
    finally {
      if (current()) {
        pending = null;
        update();
      }
    }
  };
  const documentFor = (text, replace) => {
    let document;
    try { document = JSON.parse(text); }
    catch { throw Error("The document is not valid JSON."); }
    const payload = JSON.stringify({ method: "definitions_import", params: { document, replace, dry_run: false } });
    if (new TextEncoder().encode(payload).length > limit) throw Error("The definitions request exceeds the 1 MiB service limit.");
    return document;
  };
  const showReport = (r) => {
    result.replaceChildren(
      h("p", { id: "tr-defs-status", role: "status", class: r.issues.length ? "alert error" : "alert ok", text: r.applied ? "Imported." : r.issues.length ? "Refused: nothing was written." : "Dry run: nothing was written." }),
      r.issues.length ? h("ul", {}, r.issues.map((i) => h("li", { text: i }))) : "",
      h("div", { class: "scroll", role: "region", "aria-label": "Definition import changes", tabindex: "0" },
        table(["Kind", "Identity", "Action"], [...r.work.map((w) => h("tr", {}, td("work"), td(w.id), td(w.action))), ...r.schedules.map((x) => h("tr", {}, td("schedule"), td(x.name), td(x.action)))])));
  };

  box.addEventListener("input", () => invalidate());
  replace.addEventListener("change", () => invalidate());
  file.addEventListener("change", () => {
    const selected = file.files[0];
    if (!selected) return;
    file.value = "";
    operate("read", "Reading JSON file…", async (current) => {
      if (!/\.json$/i.test(selected.name)) throw Error("Choose a .json file.");
      if (selected.size > limit) throw Error("The JSON file exceeds the 1 MiB service limit.");
      let bytes, text;
      try { bytes = await selected.arrayBuffer(); }
      catch { throw Error("Could not read the JSON file."); }
      if (!current()) return;
      try { text = new TextDecoder("utf-8", { fatal: true }).decode(bytes); }
      catch { throw Error("The JSON file is not valid UTF-8."); }
      documentFor(text, replace.checked);
      box.value = text;
      alertBox(notice, "Loaded " + selected.name + ". " + hint, "info");
    });
  });
  const exportButton = h("button", { type: "button", id: "tr-defs-export", text: "Export", onclick: () =>
    operate("export", "Exporting definitions…", async (current) => {
      const exported = await F.call("definitions_export");
      if (!current()) return;
      box.value = JSON.stringify(exported, null, 2);
      alertBox(notice, hint, "info");
    }) });
  const validate = h("button", { type: "button", id: "tr-defs-dry", text: "Validate (dry run)", onclick: () => {
    const draft = { text: box.value, replace: replace.checked };
    operate("preview", "Validating definitions…", async (current) => {
      const document = documentFor(draft.text, draft.replace);
      const r = await F.call("definitions_import", { document, replace: draft.replace, dry_run: true });
      if (!current()) return;
      if (r.dry_run !== true || r.applied !== false || r.replace !== draft.replace) {
        throw Error("The service did not return a matching dry-run result.");
      }
      showReport(r);
      alertBox(notice, "");
      if (r.issues.length === 0) preview = { ...draft, document };
    });
  } });
  const apply = h("button", { type: "button", id: "tr-defs-import", class: "primary", text: "Import", disabled: true, onclick: () => {
    if (!canImport()) return;
    const approved = preview;
    operate("import", "Importing definitions…", async (current) => {
      const r = await F.call("definitions_import", { document: approved.document, replace: approved.replace, dry_run: false });
      if (!current()) return;
      showReport(r);
      alertBox(notice, "");
    });
  } });
  return h("div", { class: "card", id: "tr-definitions" }, h("h3", { text: "Work and schedule definitions" }),
    h("p", { class: "hint", text: "Choose a JSON file or paste a document. Validate to preview changes, then select Import to apply them. Replace is required for collisions." }),
    h("label", {}, "Choose a JSON file", file),
    h("label", { class: "wide" }, "Definitions JSON", box),
    h("div", { class: "row-actions" }, exportButton, validate, apply,
      h("label", {}, replace, " Replace colliding identities")),
    notice, result);
}

registerView("overview", () => overview());
registerView("agents", () => agents());
registerView("approvals", (r) => approvals(r.query));
registerView("skills", () => skills());
registerView("providers", () => providers());
registerView("settings", () => settings());
registerView("transfer", () => transfer());

// ------------------------------------------------------------------ run detail additions
// Persistent per-run panels: the run page re-renders while a run is active, so panels
// that hold typed input, a live stream or scroll position are cached and re-attached.
const cache = { runId: null, panels: new Map(), abort: null };
const agentStates = new Map();
function panel(runId, key, signature, build) {
  if (cache.runId !== runId) {
    cache.abort?.abort();
    cache.runId = runId; cache.panels = new Map(); cache.abort = null;
  }
  const have = cache.panels.get(key);
  if (have && have.signature === signature) return have.el;
  const el = build();
  cache.panels.set(key, { signature, el });
  return el;
}
window.addEventListener("hashchange", () => {
  const r = U.parseHash();
  if (r.view !== "runs" || r.id !== cache.runId) { cache.abort?.abort(); cache.runId = null; cache.panels = new Map(); cache.abort = null; }
});

function effectsTable(run) {
  const list = run.effects || [];
  if (!list.length) return null;
  return h("div", { class: "card", id: "run-effects" }, h("h3", { text: "External effects" }),
    table(["Step", "State", "Kind", "Output", "Reconciliation"], list.map((e) => h("tr", { "data-effect": e.effect_id },
      td(h("code", { text: e.effect_id })), td(chip(e.state === "uncertain" ? "RECOVERY_REQUIRED" : e.state.toUpperCase())), td(e.action?.type || "-"),
      td(h("code", { text: brief(e.output && e.output.result !== undefined ? e.output.result : e.output, 140) })),
      td(e.reconciliation ? e.reconciliation.actor + ": " + e.reconciliation.decision + " — " + e.reconciliation.note : "-")))));
}

function reconcilePanel(run, diag) {
  const unresolved = (run.effects || []).filter((e) => e.state === "uncertain");
  const legacy = run.effect && run.effect.state === "uncertain" && !unresolved.length;
  const sig = unresolved.map((e) => e.effect_id).join("|") + (legacy ? "legacy" : "");
  return panel(run.run_id, "reconcile", sig, () => {
    const effect = h("select", { id: "rc-effect" }, unresolved.map((e) => h("option", { value: e.effect_id, text: e.effect_id })));
    const decision = h("select", { id: "rc-decision" },
      h("option", { value: "completed", text: "applied (the operation took effect)" }),
      h("option", { value: "not_applied", text: "not_applied (it did not take effect)" }));
    const note = h("textarea", { id: "rc-note", rows: 3, placeholder: "Evidence note (required): what you checked and found", maxlength: 4096 });
    const output = h("textarea", { id: "rc-output", rows: 3, text: "null" });
    const settled = h("input", { type: "checkbox", id: "rc-settled" });
    const result = h("div", { id: "rc-result" });
    return h("div", { class: "card", id: "reconcile-panel" },
      h("h3", { text: "Reconcile an unknown effect" }),
      h("p", { text: "A run stopped while an external operation was in flight, so Forge cannot know whether it happened. Check the provider, then record what you found. Cancellation or a timeout is not evidence. Nothing is replayed." }),
      h("div", { class: "row-actions" },
        unresolved.length ? h("label", {}, "Effect", effect) : "", h("label", {}, "Finding", decision)),
      h("label", { class: "wide" }, "Evidence note", note),
      h("label", { class: "wide" }, "Verified output JSON (for applied; null when none)", output),
      h("label", { class: "inline" }, settled, " The external operation has settled (provider evidence checked)"),
      h("div", { class: "row-actions" }, h("button", { type: "button", id: "rc-submit", class: "primary", text: "Record reconciliation", onclick: () => guarded(result, async () => {
        if (!note.value.trim()) throw Error("An evidence note is required.");
        if (!settled.checked) throw Error("Confirm that the external operation has settled.");
        let out = null;
        try { out = JSON.parse(output.value || "null"); } catch { throw Error("Verified output must be valid JSON."); }
        const params = { run_id: run.run_id, decision: decision.value, note: note.value.trim(), output: out, settled: true };
        if (unresolved.length) params.effect_id = effect.value;
        await F.call("reconcile", params);
        cache.runId = null;
        go("#/runs/" + encodeURIComponent(run.run_id));
      }) })), result);
  });
}

function approvalsForRun(run, diag, items) {
  const mine = items.filter((a) => a.run_id === run.run_id);
  if (!mine.length) return null;
  const sig = mine.map((a) => a.approval_id + a.state + a.actionable).join("|");
  return panel(run.run_id, "approvals", sig, () => h("div", { class: "card", id: "run-approvals" }, h("h3", { text: "Approvals" }),
    table(["State", "Request", "Decision"], mine.map((a) => h("tr", { "data-approval": a.approval_id, "data-state": a.state },
      td(approvalState(a)), td(h("strong", { text: a.title }), a.description ? h("div", { class: "hint", text: a.description }) : ""),
      td(a.state === "pending" ? (a.actionable ? approvalControls(a, diag, async () => { cache.runId = null; U.route(); }) : "run no longer waiting")
        : (a.actor || "") + ": " + (a.note || "")))))));
}

// ---- live harness/ACP transcript
function agentPanel(run, diag) {
  const steps = (run.effects || []).filter((e) => e.action && e.action.type === "harness");
  if (!steps.length) return null;
  return panel(run.run_id, "agent", steps.map((s) => s.effect_id).join("|"), () => {
    const state = { seen: new Set(), sections: new Map(), cursor: new Map(), mode: "init", ended: false, runId: run.run_id };
    const status = h("p", { id: "acp-stream-state", class: "hint", text: "Loading transcript…" });
    const root = h("div", { class: "card", id: "acp-panel" }, h("h3", { text: "Agent run" }), status);
    state.root = root; state.status = status;
    agentStates.set(run.run_id, state);
    for (const s of steps) section(state, s.effect_id, s);
    start(state, run, diag);
    return root;
  });
}
function section(state, step, effect) {
  if (state.sections.has(step)) return state.sections.get(step);
  const attempt = effect?.harness_attempt || {};
  const out = effect?.output?.harness || {};
  const text = h("pre", { class: "transcript", id: "acp-transcript", "aria-label": "Streamed agent output" });
  const toolBody = h("tbody", {}), permBody = h("tbody", {});
  const meta = h("table", { class: "acp-meta" }, h("tbody", {},
    ...[["Runner", effect?.action?.input?.profile], ["Adapter", out.adapter || attempt.agent?.info?.name], ["Transport", attempt.transport || out.transport],
      ["Session", attempt.session_id || out.session_id], ["Model", out.served_model || out.requested_model], ["Attempt", attempt.state]]
      .filter(([, v]) => v).map(([k, v]) => h("tr", {}, h("th", { text: k }), td(String(v))))));
  const el = h("div", { class: "acp-step", "data-step": step },
    h("h4", { text: "Step " + step }), meta,
    h("h5", { text: "Transcript" }), text,
    h("h5", { text: "Tool events" }),
    h("table", { id: "acp-tools" }, h("thead", {}, h("tr", {}, ["Tool call", "Kind", "Status", "Title"].map((t) => h("th", { text: t })))), toolBody),
    h("h5", { text: "Permission decisions" }),
    h("p", { class: "hint", text: "Forge answers every ACP permission request by deny-by-default policy; none waits for a human." }),
    h("table", { id: "acp-permissions" }, h("thead", {}, h("tr", {}, ["Kind", "Locations", "Decision", "Reason", "Title"].map((t) => h("th", { text: t })))), permBody));
  const sec = { el, text, toolBody, permBody, tools: new Map(), permissions: new Map(), empty: { tool: true, perm: true } };
  for (const record of attempt.permission_decisions || out.permission_decisions || []) {
    if (record.evidence_id != null) renderPermission(sec, record);
  }
  state.sections.set(step, sec);
  state.root.append(el);
  return sec;
}
function emptyRow(body, cols, label) { body.replaceChildren(h("tr", { class: "empty-row" }, h("td", { colspan: String(cols), class: "hint", text: label }))); }
function renderPermission(sec, b) {
  const key = b.evidence_id;
  const prior = key == null ? null : sec.permissions.get(key);
  if (prior && prior.dataset.disposition !== "unconfirmed" && b.disposition === "unconfirmed") return;
  if (sec.empty.perm) { sec.permBody.replaceChildren(); sec.empty.perm = false; }
  const locs = Array.isArray(b.locations) ? b.locations.map((l) => (l && l.path) || l).join(", ") : "";
  const decision = b.decision === "allowed_once" && b.disposition !== "released" ? "unconfirmed" : (b.decision || "unknown");
  const detail = b.disposition_reason || (decision === "unconfirmed" ? "Transport release was not confirmed." : b.reason);
  const row = h("tr", { "data-decision": decision, "data-disposition": b.disposition || "legacy" },
    td(b.kind || "other"), td(locs || "(none given)"), td(chip(decision === "evidence_incomplete" ? "INCOMPLETE" : decision.toUpperCase().replace("ALLOWED_ONCE", "ALLOWED"))), td(detail || "-"), td(b.title || ""));
  if (prior) prior.replaceWith(row);
  else sec.permBody.append(row);
  if (key != null) sec.permissions.set(key, row);
}
function apply(state, entry) {
  if (!entry || !entry.id || state.seen.has(entry.id)) return;
  state.seen.add(entry.id);
  const sec = section(state, entry.step, null);
  const b = entry.body || {};
  if (entry.type === "agent_message_chunk") {
    const t = b.content && b.content.text;
    if (typeof t === "string") { sec.text.textContent += t; sec.text.scrollTop = sec.text.scrollHeight; }
  } else if (entry.type === "tool_call" || entry.type === "tool_call_update") {
    const key = b.toolCallId; if (!key) return;
    let row = sec.tools.get(key);
    if (!row) {
      if (sec.empty.tool) { sec.toolBody.replaceChildren(); sec.empty.tool = false; }
      row = { kind: td("other"), status: td("pending"), title: td(""), tr: null };
      row.tr = h("tr", { "data-tool": key }, td(h("code", { text: String(key) })), row.kind, row.status, row.title);
      sec.tools.set(key, row); sec.toolBody.append(row.tr);
    }
    if (typeof b.kind === "string") row.kind.textContent = b.kind;
    if (typeof b.status === "string") row.status.textContent = b.status;
    if (typeof b.title === "string") row.title.textContent = b.title;
  } else if (entry.type === "permission") {
    renderPermission(sec, b);
  } else if (entry.type === "cancel_sent") {
    sec.text.textContent += "\n[cancel requested]\n";
  }
}
function finish(state, label) {
  state.status.textContent = label;
  for (const sec of state.sections.values()) {
    if (sec.empty.tool) emptyRow(sec.toolBody, 4, "No tool events.");
    if (sec.empty.perm) emptyRow(sec.permBody, 5, "No permission requests.");
  }
}
async function pollLogs(state) {
  for (const step of state.sections.keys()) {
    for (let page = 0; page < 20; page++) {
      const after = state.cursor.get(step) || 0;
      const r = await F.call("run_logs", { run_id: state.runId, source: "harness:" + step, after, limit: 200 });
      for (const e of r.entries) apply(state, e);
      state.cursor.set(step, r.next_after);
      if (r.complete) break;
    }
  }
}
async function start(state, run, diag) {
  const active = U.ACTIVE.includes(run.status);
  if (!active) {
    try { await pollLogs(state); finish(state, "Run is " + run.status + ". Transcript loaded from retained evidence."); }
    catch (e) { finish(state, "Transcript unavailable: " + e.message); }
    return;
  }
  const ctl = new AbortController();
  cache.abort = ctl;
  state.mode = "stream";
  state.status.textContent = "Connecting to the live event stream…";
  try {
    const response = await F.stream(state.runId, ctl.signal);
    if (!response.ok || !response.body) throw Error("stream refused (" + response.status + ")");
    state.status.textContent = "Live: streaming events as they happen.";
    const reader = response.body.getReader(), decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let at;
      while ((at = buffer.indexOf("\n\n")) >= 0) {
        const frame = buffer.slice(0, at); buffer = buffer.slice(at + 2);
        let event = "message", data = "";
        for (const line of frame.split("\n")) {
          if (line.startsWith("event: ")) event = line.slice(7);
          else if (line.startsWith("data: ")) data += line.slice(6);
        }
        if (!data) continue;
        let parsed; try { parsed = JSON.parse(data); } catch { continue; }
        if (event === "end") { state.ended = true; finish(state, "Run ended: " + (parsed.status || "complete") + "."); U.route(); return; }
        if (event === "state") { state.status.textContent = "Live: run is " + parsed.status + "."; }
        else if (event === "approval") { /* the approvals panel refreshes with the run page */ }
        else if (event !== "effect") apply(state, parsed);
      }
    }
    if (!state.ended) { state.mode = "poll"; state.status.textContent = "Live stream closed; refreshing by polling."; }
  } catch (e) {
    if (ctl.signal.aborted) return;
    state.mode = "poll";
    state.status.textContent = "Live stream unavailable (" + e.message + "); refreshing by polling.";
  }
}

// ---- logs and diagnostics
function logsPanel(run, diag) {
  return panel(run.run_id, "logs", "logs:" + run.status + ":" + (run.effects || []).length, () => {
    const card = h("div", { class: "card", id: "run-logs" }, h("h3", { text: "Logs and diagnostics" }));
    const box = h("div", {}), entries = h("tbody", {}), more = h("div", {});
    const state = { source: "audit", cursor: 0 };
    const load = (reset) => guarded(diag, async () => {
      if (reset) { state.cursor = 0; entries.replaceChildren(); }
      const r = await F.call("run_logs", { run_id: run.run_id, source: state.source, after: state.cursor, limit: 100 });
      for (const e of r.entries) entries.append(h("tr", { "data-seq": String(e.seq) }, td(ms(e.ms)), td(h("code", { text: e.type })), td(h("code", { text: brief(e.body, 400) }))));
      state.cursor = r.next_after;
      more.replaceChildren(r.complete ? h("span", { class: "hint", text: r.total + " entr" + (r.total === 1 ? "y" : "ies") + " (complete)" }) : h("button", { type: "button", text: "Load more", onclick: () => load(false) }));
    });
    F.call("run_logs", { run_id: run.run_id }).then((index) => {
      const select = h("select", { id: "log-source" }, index.sources.map((s) => h("option", { value: s.source, text: s.source + " (" + s.entries + ")" })));
      select.onchange = () => { state.source = select.value; load(true); };
      const attempts = index.sources.filter((s) => s.attempts);
      box.replaceChildren(h("label", {}, "Source", select),
        attempts.length ? h("div", {}, h("h4", { text: "Attempt diagnostics" }), table(["Step", "Attempt", "State", "Result", "Exit", "Failure / stderr"], attempts.flatMap((s) => s.attempts.map((a) => h("tr", {},
          td(s.step), td(String(a.attempt)), td(a.state || "-"), td(a.result || "-"), td(a.exit_code == null ? "-" : String(a.exit_code)),
          td(h("code", { text: brief(a.failure || a.protocol_error || a.stderr_tail || "-", 300) }))))), { id: "log-attempts" })) : "",
        h("table", { id: "log-entries" }, h("thead", {}, h("tr", {}, ["Time", "Type", "Detail"].map((t) => h("th", { text: t })))), entries), more);
      load(true);
    }).catch((e) => alertBox(diag, e.message));
    card.append(box);
    return card;
  });
}

F.runExtras = async (run, body, diag) => {
  const append = (el) => { if (el) body.append(el); };
  append(effectsTable(run));
  if (run.actions && run.actions.reconcile) append(reconcilePanel(run, diag));
  let items = [];
  try { items = (await F.call("approvals", { limit: 100 })).approvals; } catch { /* approvals are optional detail */ }
  append(approvalsForRun(run, diag, items));
  append(agentPanel(run, diag));
  append(logsPanel(run, diag));
  // Polling fallback: while the page re-renders an active run whose stream is not
  // delivering, read the retained log pages instead; once the run is terminal, drain
  // whatever the stream missed.
  const st = agentStates.get(run.run_id);
  if (st && !st.ended && st.mode === "poll") {
    try {
      await pollLogs(st);
      if (!U.ACTIVE.includes(run.status)) { st.ended = true; finish(st, "Run ended: " + run.status + "."); }
    } catch (e) { st.status.textContent = "Transcript unavailable: " + e.message; }
  }
};

U.route();

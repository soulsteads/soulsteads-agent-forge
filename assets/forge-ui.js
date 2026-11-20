// Workflows and Runs operator views. No framework: hash-routed views over the
// same canonical /api methods used by the CLI and MCP. All data is rendered with
// textContent; the bearer token lives only in the page's memory (see dashboard.html).
const F = window.forge;
const $ = (id) => document.getElementById(id);
const STATUSES = ["ADMITTED","PENDING","ENQUEUED","DELAYED","SUCCESS","ERROR","CANCELLED","MAX_RECOVERY_ATTEMPTS_EXCEEDED","RECOVERY_REQUIRED"];
const ACTIVE = ["ADMITTED","PENDING","ENQUEUED","DELAYED"];
let snapshot = null;
let pollTimer = null;
let pluginDraft = null;

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "text") el.textContent = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) if (c !== null && c !== undefined && c !== false) el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  return el;
}
const when = (ms) => (typeof ms === "number" ? new Date(ms < 1e12 ? ms * 1000 : ms).toLocaleString() : "-");
const dur = (s) => (s.started_at != null && s.completed_at != null ? ((s.completed_at - s.started_at) / 1000).toFixed(2) + "s" : "-");
const chip = (state) => h("span", { class: "chip st-" + String(state || "").toLowerCase(), text: state || "-" });
function brief(v, n = 90) {
  if (v === null || v === undefined) return "-";
  let t = typeof v === "string" ? v : v && typeof v === "object" && Object.keys(v).length === 1 && typeof v.text === "string" ? v.text : JSON.stringify(v);
  return t.length > n ? t.slice(0, n - 1) + "…" : t;
}
function pre(v, attrs = {}) { return h("pre", { ...attrs, text: typeof v === "string" ? v : JSON.stringify(v, null, 2) }); }
function scrollTable(label, table) {
  return h("div", { class: "scroll operator-table", role: "region", "aria-label": label, tabindex: "0" }, table);
}
function newKey() { return "ui-" + crypto.randomUUID(); }
function alertBox(target, message, kind = "error") {
  target.replaceChildren(message ? h("div", { class: "alert " + kind, role: kind === "error" ? "alert" : "status", text: message }) : "");
}
// Shows the service diagnostic in place; the failure is handled, not rethrown.
async function guarded(target, fn) {
  try { alertBox(target, ""); return await fn(); }
  catch (e) { alertBox(target, e.message || String(e)); return undefined; }
}
function go(hash) { if (location.hash === hash) route(); else location.hash = hash; }
function parseHash() {
  const raw = location.hash.replace(/^#/, "");
  if (!raw.startsWith("/")) return { view: "overview" };
  const [path, query] = raw.slice(1).split("?");
  const parts = path.split("/").map(decodeURIComponent);
  return { view: parts[0] || "overview", id: parts[1], rest: parts[2], query: new URLSearchParams(query || "") };
}

// ------------------------------------------------------------------ routing
function setupNavigation() {
  const nav = $("forge-navigation"), toggle = $("forge-navigation-toggle");
  if (!nav || !toggle) return;
  const narrow = matchMedia("(max-width: 760px)");
  const setOpen = (open) => {
    nav.hidden = !open;
    toggle.setAttribute("aria-expanded", String(open));
  };
  const updateWidth = () => {
    const focus = document.activeElement;
    toggle.hidden = !narrow.matches;
    setOpen(!narrow.matches);
    if (narrow.matches && nav.contains(focus)) toggle.focus();
    if (!narrow.matches && focus === toggle) {
      (nav.querySelector('[aria-current="page"]') || nav.querySelector("a"))?.focus();
    }
  };
  narrow.addEventListener("change", updateWidth);
  updateWidth();
  toggle.addEventListener("click", () => setOpen(nav.hidden));
  nav.addEventListener("click", (event) => {
    if (narrow.matches && event.target.closest("a[data-view]") && !event.defaultPrevented &&
        event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey) {
      setOpen(false);
      $("forge-main").focus();
    }
  });
  toggle.parentElement.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && narrow.matches && !nav.hidden) {
      event.preventDefault();
      setOpen(false);
      toggle.focus();
    }
  });
}
setupNavigation();

function markCurrentView(view) {
  const link = location.hash.split("?")[0];
  for (const a of document.querySelectorAll("#forge-navigation a[data-view]")) {
    const active = view === "x" ? a.getAttribute("href") === link : a.dataset.view === view;
    a.setAttribute("aria-current", active ? "page" : "false");
  }
}

function show(view) {
  for (const s of document.querySelectorAll("main > section.ops")) s.hidden = view !== "operations";
  for (const s of document.querySelectorAll('main > section[id^="view-"]')) s.hidden = s.id !== "view-" + view;
  markCurrentView(view);
}
// ------------------------------------------------------------------ view registry
// Each route is a view name -> async loader(route). Core views and plugin pages
// (`#/x/<plugin>/<page>`) both load through it.
const views = new Map();
function registerView(name, loader) { views.set(name, loader); }
// Enabled installed plugins and the dashboard routes their manifests declare.
const installedPlugins = () => (snapshot?.health?.installed_plugins || []).filter((p) => p.enabled);
function renderPluginNav() {
  const links = $("forge-plugin-links");
  links.replaceChildren();
  for (const plugin of installedPlugins()) {
    for (const r of plugin.dashboard || []) {
      links.append(h("li", {}, h("a", {
        href: "#/x/" + encodeURIComponent(plugin.id) + "/" + encodeURIComponent(r.path),
        "data-view": "x", "data-plugin-nav": plugin.id,
      }, h("span", { class: "nav-icon", "aria-hidden": "true", text: "◇" }), h("span", { text: r.label }))));
    }
  }
  $("forge-plugin-navigation").hidden = links.childElementCount === 0;
  markCurrentView(parseHash().view);
}
let activePlugin = null;
function disposePlugin(cleanup) {
  if (typeof cleanup !== "function") return;
  try { Promise.resolve(cleanup()).catch(() => {}); } catch { /* plugin cleanup is best effort */ }
}
function cleanupPlugin() {
  const previous = activePlugin;
  activePlugin = null;
  disposePlugin(previous?.cleanup);
}
async function pluginPage(pluginId, page) {
  const body = $("x-body"), diag = $("x-alert");
  const generation = routeGeneration;
  const isCurrent = () => {
    const route = parseHash();
    return generation === routeGeneration && route.view === "x" && route.id === pluginId && route.rest === page;
  };
  try {
    cleanupPlugin();
    body.replaceChildren();
    const nextSnapshot = await F.call("snapshot");
    if (!isCurrent()) return;
    snapshot = nextSnapshot;
    renderPluginNav();
    const plugin = installedPlugins().find((p) => p.id === pluginId);
    const route = plugin?.dashboard?.find((r) => r.path === page);
    if (!route) throw Error("This plugin page is not available: the plugin is disabled, not installed, or does not declare this route.");
    if (route.stylesheet && !document.querySelector('link[data-plugin-css="' + pluginId + "/" + page + '"]')) {
      document.head.append(h("link", { rel: "stylesheet", href: route.stylesheet, "data-plugin-css": pluginId + "/" + page }));
    }
    const mount = h("div", { id: "plugin-mount", "data-plugin": pluginId });
    body.append(h("h2", { text: route.label }), mount);
    // Trusted first-party model: the module runs with this page's authority. It gets only
    // this small host API; third-party sandboxing is a documented non-goal for now.
    const host = Object.freeze({
      pluginId, route: page, mount,
      fetch: async (subpath, init) => {
        if (!isCurrent()) throw Error("This plugin page is no longer active.");
        return F.pluginFetch(pluginId, subpath, init);
      },
      setStatus: (text) => { if (isCurrent()) F.status(String(text)); },
    });
    const module = await import(route.entry + "?v=" + encodeURIComponent(plugin.version));
    if (!isCurrent()) return;
    const mountFn = module.mount || module.default;
    if (typeof mountFn !== "function") throw Error("The plugin module exports no mount function.");
    const cleanup = await mountFn(mount, host);
    if (isCurrent()) activePlugin = { cleanup: typeof cleanup === "function" ? cleanup : null };
    else disposePlugin(cleanup);
  } catch (error) {
    if (isCurrent()) alertBox(diag, error.message || String(error));
  }
}
const ALERT_IDS = { runs: "runs-alert", schedules: "sc-alert", queues: "q-alert", plugins: "pl-alert", x: "x-alert", workflows: "wf-alert" };
let routeGeneration = 0;
const alertFor = (view) => $(ALERT_IDS[view] || view + "-alert");
async function route() {
  ++routeGeneration;
  cleanupPlugin();
  clearTimeout(pollTimer);
  const r = parseHash();
  show(r.view);
  if (!F.connected()) {
    const target = alertFor(r.view);
    if (r.view !== "operations" && target) alertBox(target, "Connect with a scoped bearer token above to load canonical state.", "info");
    return;
  }
  const target = alertFor(r.view);
  if (target) alertBox(target, "");
  try {
    const loader = views.get(r.view);
    if (loader) await loader(r);
  } catch (e) {
    if (target) alertBox(target, e.message || String(e));
    F.status("Failed / state may be stale: " + e.message);
  }
}
registerView("workflows", (r) => (r.id ? workflowDetail(r.id) : workflowList()));
registerView("runs", (r) => (r.id ? runDetail(r.id) : runList(r.query)));
registerView("schedules", (r) => (r.id ? scheduleDetail(r.id) : scheduleList(r.query)));
registerView("queues", (r) => queueView(r.id, r.query));
registerView("plugins", (r) => pluginView(r.id));
registerView("x", (r) => pluginPage(r.id, r.rest));
window.addEventListener("hashchange", route);
F.onConnect.push(async (s) => { snapshot = s; renderPluginNav(); if (parseHash().view !== "operations") await route(); });

// ------------------------------------------------------------------ plugins
// Minimal lifecycle management; the complete plugin UI belongs to a later milestone.
function installedTable(list) {
  if (!list.length) return h("p", { class: "empty", id: "pl-installed-empty", text: "No plugins installed. Install one with `agent-forge plugins install DIR`; installs start disabled." });
  const act = (label, fn) => h("button", { type: "button", text: label, onclick: () => guarded($("pl-alert"), async () => { await fn(); await pluginView(); }) });
  return h("table", { id: "pl-installed" },
    h("thead", {}, h("tr", {}, h("th", { text: "Plugin" }), h("th", { text: "Version" }), h("th", { text: "State" }), h("th", { text: "Health" }), h("th", { text: "Actions" }))),
    h("tbody", {}, list.map((p) => h("tr", { "data-plugin": p.id },
      h("td", { text: p.id + " — " + p.description }),
      h("td", { text: p.version }),
      h("td", { text: p.state }),
      h("td", { text: p.problem || "ok" }),
      h("td", {},
        p.enabled ? act("Disable", () => F.call("plugin_disable", { id: p.id })) : act("Enable", () => F.call("plugin_enable", { id: p.id })),
        p.enabled ? "" : act("Uninstall", () => F.call("plugin_uninstall", { id: p.id })),
        h("button", { type: "button", text: "Verify", onclick: () => guarded($("pl-alert"), async () => {
          const r = await F.call("plugin_show", { id: p.id, check: true });
          alertBox($("pl-alert"), "Health of " + p.id + ": " + JSON.stringify(r.health), r.health && r.health.error ? "error" : "ok");
        }) }))))));
}
// Install from a directory on the service's host: a dry run validates the manifest,
// digests and collisions; an install stages, verifies and publishes it DISABLED.
function pluginInstallForm() {
  const source = h("input", { id: "pl-source", required: true, size: 50, placeholder: "/absolute/path/to/plugin-directory" });
  const result = h("div", { id: "pl-install-result" });
  const run = (dry) => guarded(result, async () => {
    if (!source.value.trim()) throw Error("Enter the plugin directory on the service host.");
    const r = await F.call("plugin_install", { source: source.value.trim(), dry_run: dry });
    alertBox(result, dry ? "Valid: " + JSON.stringify(r) : "Installed disabled: " + JSON.stringify(r.id ? { id: r.id, version: r.version, state: r.state } : r), "ok");
    if (!dry) await pluginView();
  });
  return h("div", { class: "card", id: "pl-install" },
    h("h3", { text: "Install a plugin" }),
    h("p", { class: "hint", text: "The directory must contain a manifest.json with matching sha256 digests. Installation copies it under the plugins directory, verifies it and leaves it disabled; enabling is a separate step. Disable is refused while the plugin has active runs; uninstall is refused while work references it." }),
    h("label", {}, "Plugin directory (on the service host)", source),
    h("div", { class: "row-actions" },
      h("button", { type: "button", id: "pl-check", text: "Validate (dry run)", onclick: () => run(true) }),
      h("button", { type: "button", id: "pl-install-go", class: "primary", text: "Install (disabled)", onclick: () => run(false) })),
    result);
}
async function pluginView(kind) {
  const body = $("pl-body"), diag = $("pl-alert");
  body.replaceChildren();
  await guarded(diag, async () => {
    snapshot = await F.call("snapshot");
    renderPluginNav();
    const types = Object.entries(snapshot.types).filter(([name]) => name.startsWith("plugin."));
    if (!kind) {
      body.replaceChildren(
        h("h2", { text: "Configured plugins" }),
        h("p", { text: "Executable capabilities configured by your operator. Descriptions and schemas come from the plugins and are cached when this service starts. Inspection does not run a plugin." }),
        h("h2", { text: "Installed plugins" }),
        installedTable(snapshot.health.installed_plugins || []),
        pluginInstallForm(),
        types.length ? h("table", { id: "pl-table" },
          h("thead", {}, h("tr", {}, h("th", { text: "Type" }), h("th", { text: "Description" }))),
          h("tbody", {}, types.map(([name, entry]) => h("tr", {},
            h("td", {}, h("a", { href: "#/plugins/" + encodeURIComponent(name), text: name })),
            h("td", { text: entry.description })))))
          : h("p", { class: "empty", text: "No executable plugins configured. Ask your operator to configure a trusted executable and grant its type before authoring plugin work." }),
        h("p", {}, h("a", { href: "#/workflows", text: "Workflows" }), " · ", h("a", { href: "#/runs", text: "Runs and outcomes" })),
        h("p", { class: "hint", text: "No plugins are bundled. Package installation, lifecycle management, settings editing and visibility controls are not available here." }));
      return;
    }
    const metadata = types.find(([name]) => name === kind)?.[1];
    body.append(h("p", {}, h("a", { href: "#/plugins", text: "← Configured plugins" })));
    if (!metadata) throw Error("Plugin type is not configured in this service; open Configured plugins to choose a supported type.");
    const input = h("textarea", { id: "pl-input", rows: 8, text: "{}" });
    const validation = h("div", { id: "pl-diag" });
    const validate = (author) => guarded(validation, async () => {
      let value;
      try { value = JSON.parse(input.value); }
      catch { throw Error("Input is not valid JSON. Correct the JSON syntax and try again."); }
      await F.call("validate", { definition: { type: kind, input: value } });
      alertBox(validation, "Valid input under registration policy. Nothing stored or executed. Execution requires execute authority.", "ok");
      if (author) { pluginDraft = { type: kind, input: value }; go("#/workflows?mode=new"); }
    });
    body.append(h("h2", { text: kind }), h("p", { id: "pl-description", text: metadata.description }),
      h("h3", { text: "Input schema" }), h("pre", { id: "pl-schema", text: JSON.stringify(metadata.schema, null, 2) }),
      h("p", { text: "Validate input using the same policy and schema checks as work registration. This does not call the executable. A valid input can be carried into a new workflow step." }),
      h("label", { class: "wide" }, "Input JSON", input),
      h("div", { class: "row-actions" },
        h("button", { type: "button", id: "pl-validate", text: "Validate input", onclick: () => validate(false) }),
        h("button", { type: "button", id: "pl-author", text: "Use in new workflow", onclick: () => validate(true) })),
      validation,
      h("p", {}, h("a", { href: "#/workflows", text: "Workflows" }), " · ", h("a", { href: "#/runs", text: "Runs and outcomes" })));
  });
}

// ------------------------------------------------------------------ workflows
let workflowListRead = 0;
function workflowHeading(label, title, description, ...actions) {
  return h("header", { class: "wf-heading" }, h("div", {},
    h("p", { class: "eyebrow", text: label }), h("h2", { text: title }), h("p", { text: description })),
    h("div", { class: "row-actions" }, actions));
}

async function workflowList() {
  const authoring = () => parseHash().query?.get("mode") === "new";
  const author = authoring();
  const onList = () => {
    const route = parseHash();
    return route.view === "workflows" && !route.id && authoring() === author;
  };
  if (!onList()) return;
  const read = ++workflowListRead, generation = routeGeneration;
  const isCurrent = () => generation === routeGeneration && read === workflowListRead && onList();
  const body = $("wf-body");
  if (!author && body.querySelector("#wf-create")) body.replaceChildren();
  let nextSnapshot;
  try {
    nextSnapshot = await F.call("snapshot");
  } catch (error) {
    if (isCurrent()) {
      const message = error.message || String(error);
      alertBox($("wf-alert"), message);
      F.status("Failed / state may be stale: " + message);
    }
    return;
  }
  if (!isCurrent()) return;
  snapshot = nextSnapshot;
  if (author) {
    body.replaceChildren(workflowHeading("Automations / Builder", "New workflow",
      "Compose a definition, validate it with Forge, then register it under a new ID.",
      h("a", { class: "action-link", href: "#/workflows", text: "← Workflow library" })), builder());
    return;
  }
  const flows = snapshot.work.filter((w) => w.definition.type === "workflow");
  const enabled = flows.filter((w) => w.enabled).length;
  const scheduleCounts = new Map();
  for (const schedule of snapshot.schedules || []) {
    const id = schedule.context.work_id;
    scheduleCounts.set(id, (scheduleCounts.get(id) || 0) + 1);
  }
  const rows = h("tbody");
  const count = h("p", { class: "hint wf-count", role: "status" });
  const empty = h("p", { id: "wf-empty", class: "empty" });
  const table = scrollTable("Workflow library", h("table", { id: "wf-table" }, h("thead", {},
    h("tr", {}, ["Workflow", "State", "Schedules", "Created", "Activity"].map((label) => h("th", { scope: "col", text: label })))), rows));
  const search = h("input", { id: "wf-search", type: "search", placeholder: "Search by workflow ID…", oninput: render });
  const state = h("select", { id: "wf-state", onchange: render },
    h("option", { value: "", text: "All states" }), h("option", { value: "enabled", text: "Enabled" }), h("option", { value: "disabled", text: "Disabled" }));
  function render() {
    const query = search.value.trim().toLowerCase();
    const filtered = flows.filter((w) => w.id.toLowerCase().includes(query) && (!state.value || (w.enabled ? "enabled" : "disabled") === state.value));
    table.hidden = filtered.length === 0;
    empty.hidden = filtered.length !== 0;
    empty.textContent = flows.length ? "No workflows match these filters." : "No workflows in this list. Create a workflow to get started.";
    count.textContent = `Showing ${filtered.length} of ${flows.length} workflows`;
    rows.replaceChildren(...filtered.map((w) => h("tr", { "data-workflow": w.id },
      h("th", { scope: "row" }, h("a", { href: "#/workflows/" + encodeURIComponent(w.id), text: w.id })),
      h("td", {}, chip(w.enabled ? "Enabled" : "Disabled")),
      h("td", { text: String(scheduleCounts.get(w.id) || 0) }),
      h("td", { text: when(w.created) }),
      h("td", {}, h("a", { href: "#/runs?work=" + encodeURIComponent(w.id), "aria-label": "View runs for " + w.id, text: "View runs →" })))));
  }
  const metric = (label, value) => h("div", { class: "wf-metric" }, h("dt", { text: label }), h("dd", { text: String(value) }));
  render();
  body.replaceChildren(
    workflowHeading("Automations / Library", "Workflows", "Find, inspect, and operate your registered workflows.",
      h("a", { class: "action-link primary", href: "#/workflows?mode=new", text: "+ New workflow" })),
    h("dl", { class: "wf-metrics", "aria-label": "Workflows in this list" },
      metric("Workflows listed", flows.length), metric("Enabled", enabled), metric("Disabled", flows.length - enabled)),
    h("section", { class: "card wf-library", "aria-labelledby": "wf-library-title" },
      h("h3", { id: "wf-library-title", text: "Workflow library" }),
      h("div", { class: "wf-toolbar" }, h("label", {}, "Find workflow", search), h("label", {}, "State", state)),
      count, empty, table,
      snapshot.limits?.work && snapshot.work.length >= snapshot.limits.work ? h("p", { class: "hint", text: `Forge returns up to ${snapshot.limits.work} work definitions. Filters and counts apply to this list only.` }) : null));
}

const EXAMPLE = {
  params: { topic: "release notes", audience: "operators" },
  steps: [
    { id: "draft", action: "echo", input: { text: "Draft for ${/params/audience}: ${/params/topic}" } },
    { id: "review", needs: ["draft"], action: "echo", input: { text: "Reviewed: ${/steps/draft/output/text}" } },
    { id: "publish", needs: ["review"], action: "echo", input: { text: "Published: ${/steps/review/output/text}" } },
  ],
  output: { summary: { $ref: "/steps/publish/output/text" } },
};

function builder() {
  const stepTypes = Object.keys(snapshot.types).filter((t) => !["agent", "harness"].includes(t));
  const works = snapshot.work.map((w) => w.id);
  const state = { steps: [] };
  const stepsBox = h("div", { id: "wf-steps" });
  const preview = h("pre", { id: "wf-preview" });
  const diag = h("div", { id: "wf-diag" });
  const idInput = h("input", { id: "wf-id", required: true, pattern: "[a-zA-Z0-9][a-zA-Z0-9_.\\-]{0,79}", placeholder: "e.g. release-notes" });
  const params = h("textarea", { id: "wf-params", rows: 3, text: "{}" });
  const output = h("textarea", { id: "wf-output", rows: 2, placeholder: '{"summary":{"$ref":"/steps/LAST/output/text"}} — empty uses the last step output' });
  const raw = h("textarea", { id: "wf-raw", rows: 12, hidden: true });

  function definition() {
    const input = { params: JSON.parse(params.value || "{}"), steps: state.steps.map((s) => {
      const step = { id: s.id.value.trim() };
      const needs = [...s.needs.querySelectorAll("input:checked")].map((c) => c.value);
      if (needs.length) step.needs = needs;
      step.action = s.kind.value === "@work" ? { work_id: s.work.value, params: JSON.parse(s.input.value || "{}") } : { type: s.kind.value, input: JSON.parse(s.input.value || "{}") };
      return step;
    }) };
    if (!Object.keys(input.params).length) delete input.params;
    if (output.value.trim()) input.output = JSON.parse(output.value);
    return { type: "workflow", input };
  }
  function refresh() {
    state.steps.forEach((s, i) => {
      s.label.textContent = "Step " + (i + 1);
      const earlier = state.steps.slice(0, i).map((x) => x.id.value.trim()).filter(Boolean);
      const checked = new Set([...s.needs.querySelectorAll("input:checked")].map((c) => c.value));
      s.needs.replaceChildren(h("span", { text: earlier.length ? "Reads from:" : "No earlier steps" }), ...earlier.map((id) => h("label", { class: "inline" }, h("input", { type: "checkbox", value: id, checked: checked.has(id), onchange: refresh }), id)));
      s.work.hidden = s.kind.value !== "@work";
    });
    try { preview.textContent = JSON.stringify(definition(), null, 2); preview.classList.remove("bad"); }
    catch (e) { preview.textContent = "Not valid JSON yet: " + e.message; preview.classList.add("bad"); }
  }
  function addStep(init = {}) {
    const kind = h("select", { class: "step-kind", onchange: () => { if (!input.value.trim() || input.dataset.auto) { input.value = kind.value === "echo" ? '{"text":""}' : "{}"; input.dataset.auto = "1"; } refresh(); } },
      stepTypes.map((t) => h("option", { value: t, text: t === "workflow" ? "nested workflow" : t })), h("option", { value: "@work", text: "saved work (by ID)" }));
    const work = h("select", { class: "step-work" }, works.map((w) => h("option", { value: w, text: w })));
    const input = h("textarea", { class: "step-input", rows: 2, oninput: () => { delete input.dataset.auto; refresh(); } });
    const s = { id: h("input", { class: "step-id", required: true, placeholder: "step id", oninput: refresh }), kind, work, input, needs: h("div", { class: "needs" }), label: h("strong") };
    s.id.value = init.id || "step" + (state.steps.length + 1);
    kind.value = init.work_id ? "@work" : init.action || "echo";
    if (init.work_id) work.value = init.work_id;
    input.value = JSON.stringify(Object.hasOwn(init, "input") && init.input !== undefined ? init.input : Object.hasOwn(init, "params") && init.params !== undefined ? init.params : (kind.value === "echo" ? { text: "" } : {}));
    const row = h("fieldset", { class: "step" }, h("legend", {}, s.label),
      h("label", {}, "ID", s.id), h("label", {}, "Action", kind), h("label", {}, "Work", work),
      h("label", { class: "wide" }, "Input JSON (use ${/params/x} or {\"$ref\":\"/steps/ID/output/..\"})", input), s.needs,
      h("div", { class: "row-actions" },
        h("button", { type: "button", text: "↑", title: "Move up", onclick: () => move(s, -1) }),
        h("button", { type: "button", text: "↓", title: "Move down", onclick: () => move(s, 1) }),
        h("button", { type: "button", class: "danger", text: "Remove", onclick: () => { state.steps.splice(state.steps.indexOf(s), 1); row.remove(); refresh(); } })));
    s.row = row;
    state.steps.push(s); stepsBox.append(row);
    if (init.needs) { refresh(); for (const c of s.needs.querySelectorAll("input")) c.checked = init.needs.includes(c.value); }
    refresh();
  }
  function move(s, d) {
    const i = state.steps.indexOf(s), j = i + d;
    if (j < 0 || j >= state.steps.length) return;
    state.steps.splice(i, 1); state.steps.splice(j, 0, s);
    stepsBox.replaceChildren(...state.steps.map((x) => x.row)); refresh();
  }
  function load(example) {
    state.steps = []; stepsBox.replaceChildren();
    params.value = JSON.stringify(example.params || {});
    output.value = example.output === undefined ? "" : JSON.stringify(example.output);
    for (const st of example.steps) addStep(st);
  }
  function fromRaw() {
    const d = JSON.parse(raw.value);
    const inp = d.type ? d.input : d;
    load({ params: inp.params, output: inp.output, steps: inp.steps.map((s) => s.action.work_id ? { id: s.id, needs: s.needs, work_id: s.action.work_id, params: s.action.params } : { id: s.id, needs: s.needs, action: s.action.type, input: s.action.input }) });
  }
  params.addEventListener("input", refresh); output.addEventListener("input", refresh);
  const submit = async (method) => guarded(diag, async () => {
    const d = definition();
    if (method === "validate") {
      const r = await F.call("validate", { definition: d });
      alertBox(diag, `Valid: ${r.steps.length} step(s) would be registered as written.`, "ok");
    } else {
      if (!idInput.reportValidity()) return;
      const w = await F.call("register", { work_id: idInput.value, definition: d });
      go("#/workflows/" + encodeURIComponent(w.id));
    }
  });
  if (pluginDraft && stepTypes.includes(pluginDraft.type)) {
    addStep({ id: "step1", action: pluginDraft.type, input: pluginDraft.input });
    pluginDraft = null;
  } else addStep({ id: "step1", action: "echo", input: { text: "${/params/topic}" } });
  return h("section", { class: "card", id: "wf-builder" },
    h("h3", { text: "Workflow definition" }),
    h("p", { class: "hint", text: "Steps run in order. A step may read parameters and the outputs of earlier steps it lists under “Reads from”. Validation uses the service's registration checks without storing anything." }),
    h("div", { class: "grid2" }, h("label", {}, "Workflow ID", idInput), h("label", {}, "Parameter defaults JSON", params)),
    stepsBox,
    h("div", { class: "row-actions" },
      h("button", { type: "button", id: "wf-add-step", text: "+ Add step", onclick: () => addStep() }),
      h("button", { type: "button", text: "Load example", onclick: () => load(EXAMPLE) }),
      h("button", { type: "button", text: "Edit as JSON", onclick: () => { raw.hidden = !raw.hidden; if (!raw.hidden) raw.value = preview.textContent; } }),
      h("button", { type: "button", text: "Apply JSON to builder", onclick: () => guarded(diag, async () => fromRaw()) })),
    raw,
    h("label", { class: "wide" }, "Output mapping JSON (optional)", output),
    h("details", {}, h("summary", { text: "Definition preview" }), preview),
    h("div", { class: "row-actions" },
      h("button", { type: "button", id: "wf-validate", text: "Validate", onclick: () => submit("validate") }),
      h("button", { type: "button", id: "wf-create", class: "primary", text: "Create workflow", onclick: () => submit("register") })),
    diag);
}

async function workflowDetail(id) {
  const body = $("wf-body");
  const generation = routeGeneration;
  const isCurrent = () => {
    const current = parseHash();
    return generation === routeGeneration && current.view === "workflows" && current.id === id;
  };
  if (!isCurrent()) return;
  body.replaceChildren(h("p", { role: "status", class: "hint", text: "Loading workflow…" }));
  let w;
  try { w = await F.call("work", { work_id: id }); }
  catch (error) { if (isCurrent()) { body.replaceChildren(); throw error; } return; }
  if (!isCurrent()) return;
  const d = w.definition;
  const runsBox = h("div", { id: "wf-recent" }, h("p", { class: "hint", text: "Loading recent runs…" }));
  const scheduleBox = h("div", { id: "wf-schedules" }, h("p", { class: "hint", text: "Loading schedules…" }));
  const diag = h("div");
  const downloadNotice = h("div");
  const download = h("button", { type: "button", text: "Download JSON", onclick: () => guarded(downloadNotice, async () => {
    const bundle = await F.call("definitions_export");
    const current = parseHash();
    if (!download.isConnected || current.view !== "workflows" || current.id !== id) return;
    const work = bundle?.format === "forge-definitions/v1" && Array.isArray(bundle.work)
      ? bundle.work.find((entry) => entry?.id === id) : null;
    if (!work) throw Error("This workflow is not available in your definitions export.");
    const data = JSON.stringify({ format: bundle.format, work: [work] }, null, 2) + "\n";
    const url = URL.createObjectURL(new Blob([data], { type: "application/json" }));
    const name = "workflow-" + id.replace(/[^A-Za-z0-9_.-]/g, "_").slice(0, 128) + ".json";
    h("a", { href: url, download: name }).click();
    setTimeout(() => URL.revokeObjectURL(url), 0);
  }) });
  const steps = (d.type === "workflow" ? d.input.steps || [] : []).map((s, i) => h("li", {},
    h("details", { class: "wf-step" }, h("summary", {},
      h("span", { class: "step-number", "aria-hidden": "true", text: String(i + 1).padStart(2, "0") }),
      h("strong", { text: s.id }), chip(s.action.work_id ? "Saved work" : s.action.type),
      h("small", { text: s.needs?.length ? "Reads from " + s.needs.join(", ") : "No step dependencies" })),
    s.action.work_id ? h("p", {}, "Saved work: ", h("code", { text: s.action.work_id })) : null,
    h("p", { class: "hint", text: s.action.work_id ? "Step parameters" : "Step input" }),
    pre(s.action.input ?? s.action.params ?? null))));
  const defaults = d.input.params || {};
  const paramsInput = h("textarea", { id: "run-params-json", rows: 4, text: JSON.stringify(defaults, null, 2) });
  const keyInput = h("input", { id: "run-key", value: newKey(), size: 44 });
  const run = () => guarded(diag, async () => {
    const r = await F.call("run", { work_id: id, key: keyInput.value, params: JSON.parse(paramsInput.value || "{}") });
    if (isCurrent() && keyInput.isConnected) go("#/runs/" + encodeURIComponent(r.run_id));
  });
  const workflow = d.type === "workflow";
  const runHistory = "#/runs?work=" + encodeURIComponent(id);
  body.replaceChildren(
    workflowHeading(workflow ? "Automations / Workflow" : "Automations / Saved work", w.id,
      workflow ? "Inspect the definition, run it on demand, and follow the evidence." : "This saved work is not a multi-step workflow.",
      h("a", { class: "action-link", href: "#/workflows", text: "← Workflow library" }), workflow ? download : null),
    downloadNotice,
    h("div", { class: "wf-meta" }, chip(w.enabled ? "Enabled" : "Disabled"), h("span", { text: "Created " + when(w.created) }),
      workflow ? h("span", { text: `${steps.length} ${steps.length === 1 ? "step" : "steps"}` }) : chip(d.type)),
    h("div", { class: "wf-detail-grid" },
    workflow ? h("section", { class: "card", id: "wf-structure", "aria-labelledby": "wf-structure-title" },
      h("h3", { id: "wf-structure-title", text: "Workflow steps" }),
      h("p", { class: "hint", text: "Expand a step to inspect its full input. Definitions are immutable; register a new ID to make changes." }),
      h("ol", { class: "wf-chain" }, steps),
      h("p", {}, "Output: ", h("code", { text: Object.hasOwn(d.input, "output") ? JSON.stringify(d.input.output) : "last step's output" })),
      h("details", {}, h("summary", { text: "Complete definition JSON" }), pre(d))) : null,
    h("section", { class: "card wf-run-card", "aria-labelledby": "wf-run-title" },
      h("h3", { id: "wf-run-title", text: "Run on demand" }),
      h("label", { class: "wide" }, "Parameters JSON (overrides defaults shallowly)", paramsInput),
      h("details", {}, h("summary", { text: "Request identity" }),
        h("label", {}, "Request key (reuse after a lost response; a new key is a new run)", keyInput),
        h("button", { type: "button", text: "New key", onclick: () => { keyInput.value = newKey(); } })),
      h("div", { class: "row-actions" },
        h("button", { type: "button", id: "wf-run", class: "primary", text: workflow ? "Run workflow" : "Run work", disabled: !w.enabled, onclick: run }),
        h("button", { type: "button", id: "wf-toggle", text: w.enabled ? "Disable" : "Enable", onclick: () => guarded(diag, async () => { await F.call("enable", { work_id: id, enabled: !w.enabled }); if (isCurrent()) await workflowDetail(id); }) })),
      diag)),
    h("section", { class: "card", "aria-labelledby": "wf-schedules-title" },
      h("div", { class: "wf-section-head" }, h("h3", { id: "wf-schedules-title", text: "Schedules" }),
        w.enabled ? h("a", { class: "action-link", href: "#/schedules?work=" + encodeURIComponent(id), text: "+ Add schedule" }) : null), scheduleBox),
    h("section", { class: "card", "aria-labelledby": "wf-recent-title" },
      h("div", { class: "wf-section-head" }, h("h3", { id: "wf-recent-title", text: "Recent runs" }),
        h("a", { href: runHistory, text: "View run history →" })), runsBox));
  async function hydrate(box, label, load) {
    try { box.replaceChildren(await load()); }
    catch (e) { alertBox(box, `${label} unavailable: ${e.message || e}`); }
  }
  await Promise.all([
    hydrate(runsBox, "Recent runs", async () => runTable((await F.call("runs", { work_id: id, limit: 10 })).runs)),
    hydrate(scheduleBox, "Schedules", async () => {
      const schedules = (await F.call("schedules")).filter((s) => s.context.work_id === id);
      return schedules.length ? h("ul", { class: "wf-schedule-list" }, schedules.map((s) => h("li", {},
        h("a", { href: "#/schedules/" + encodeURIComponent(s.schedule_name), text: s.schedule_name }), chip(s.status),
        h("code", { text: s.schedule }), h("small", { text: s.next_run ? "Next: " + iso(s.next_run) + " · " + (s.cron_timezone || "UTC") : "No upcoming tick" }))))
        : h("p", { class: "empty", text: "No schedules for this work." });
    }),
  ]);
}

// ------------------------------------------------------------------ runs
// A rejected call may have lost its response after admission. Keep its key
// for an explicit resubmission, including when the run detail is refreshed.
const retryAttempts = new Map();

function renderRetryButton(button, diag) {
  const attempt = retryAttempts.get(button.dataset.runId);
  button.disabled = Boolean(attempt?.pending);
  button.textContent = "Retry as new linked run";
  if (attempt?.pending) button.textContent = "Submitting retry…";
  if (attempt?.error) button.textContent = "Resubmit same retry";
  alertBox(diag, attempt?.error ? `${attempt.error} Resubmitting uses the same request key.` : "");
}

function retryButton(id, diag) {
  const showingRun = () => {
    const route = parseHash();
    return route.view === "runs" && route.id === id;
  };
  const button = h("button", { type: "button", id: "run-retry", "data-run-id": id, onclick: () => guarded(diag, async () => {
    const previous = retryAttempts.get(id);
    if (previous?.pending || !showingRun() || $("run-retry") !== button) return;
    const attempt = { key: previous?.key ?? newKey(), pending: true };
    retryAttempts.set(id, attempt);
    renderRetryButton(button, diag);
    let child;
    try {
      const result = await F.call("retry", { run_id: id, key: attempt.key });
      child = result?.run_id;
      if (typeof child !== "string" || !child || child === id) throw Error("The service returned no linked run ID.");
    } catch (error) {
      attempt.pending = false;
      attempt.error = error.message || String(error);
      const current = $("run-retry");
      if (showingRun() && current?.dataset.runId === id) renderRetryButton(current, $("run-retry-diag"));
      return;
    }
    retryAttempts.delete(id);
    if (showingRun()) go("#/runs/" + encodeURIComponent(child));
  }) });
  renderRetryButton(button, diag);
  return button;
}

function runTable(runs) {
  if (!runs.length) return h("p", { class: "empty", text: "No runs match." });
  return scrollTable("Run results", h("table", { class: "runs-table" },
    h("thead", {}, h("tr", {}, ["Run", "Status", "Work", "Type", "Created", "Result"].map((t) => h("th", { text: t })))),
    h("tbody", {}, runs.map((r) => h("tr", {},
      h("td", {}, h("a", { href: "#/runs/" + encodeURIComponent(r.run_id), text: r.run_id })),
      h("td", {}, chip(r.status)), h("td", { text: r.work_id }), h("td", { text: r.type || "-" }), h("td", { text: when(r.created) }),
      h("td", { text: r.failure ? (r.failure.location === "unknown" ? "failed (location unknown)" : "failed at " + (r.failure.step || "workflow") + ": " + brief(r.failure.reason, 80)) : brief(r.output) }))))));
}

async function runList(query) {
  const body = $("runs-body");
  snapshot = snapshot || (await F.call("snapshot"));
  const requestedWork = query.get("work") || "";
  const workIds = new Set(snapshot.work.map((w) => w.id));
  if (requestedWork) workIds.add(requestedWork);
  const work = h("select", { name: "work", id: "runs-work" }, h("option", { value: "", text: "Any work" }), [...workIds].map((id) => h("option", { value: id, text: id })));
  const status = h("select", { name: "status", id: "runs-status" }, h("option", { value: "", text: "Any status" }), STATUSES.map((s) => h("option", { value: s, text: s })));
  const limit = h("input", { name: "limit", type: "number", min: 1, max: 100, value: query.get("limit") || 25 });
  work.value = requestedWork; status.value = query.get("status") || "";
  const results = h("div", { id: "runs-results" });
  const more = h("div");
  const form = h("form", { id: "runs-filter", onsubmit: (e) => { e.preventDefault(); const q = new URLSearchParams(); if (work.value) q.set("work", work.value); if (status.value) q.set("status", status.value); if (limit.value) q.set("limit", limit.value); go("#/runs?" + q); } },
    h("label", {}, "Work", work), h("label", {}, "Status", status), h("label", {}, "Limit", limit), h("button", { text: "Search" }));
  body.replaceChildren(h("h2", { text: "Runs" }), h("p", { text: "Newest first. Choose a run to see each step's state, output and the reason for any failure." }), form, results, more);
  const params = { limit: Number(limit.value) || 25 };
  if (work.value) params.work_id = work.value;
  if (status.value) params.status = status.value;
  const all = [];
  const page = async (before) => guarded($("runs-alert"), async () => {
    const r = await F.call("runs", before ? { ...params, before } : params);
    all.push(...r.runs);
    results.replaceChildren(h("p", { class: "hint", text: `${all.length} run(s) shown · scanned ${r.scanned} admissions in this page` }), runTable(all));
    more.replaceChildren(r.next_before ? h("button", { type: "button", id: "runs-more", text: "Older runs", onclick: () => page(r.next_before) }) : "");
  });
  await page();
}

async function runDetail(id) {
  const body = $("runs-body");
  const diag = h("div", { id: "run-diag" });
  const showingRun = () => {
    const route = parseHash();
    return route.view === "runs" && route.id === id;
  };
  const run = await F.call("inspect", { run_id: id });
  if (!showingRun()) return;
  const retryDiag = run.actions.retry ? h("div", { id: "run-retry-diag" }) : null;
  const wf = run.workflow;
  const header = h("div", { class: "card run-head" },
    h("h2", {}, "Run ", h("code", { text: run.run_id })),
    h("p", {}, chip(run.status), " ", h("a", { href: (run.type === "workflow" ? "#/workflows/" : "#/runs?work=") + encodeURIComponent(run.work_id), text: run.work_id }), ` · ${run.type} · created ${when(run.created)}`),
    run.parent ? h("p", {}, "Retry of ", h("a", { href: "#/runs/" + encodeURIComponent(run.parent), text: run.parent })) : "",
    run.diagnostic ? h("p", { class: "alert warn", text: run.diagnostic }) : "",
    wf && wf.failure ? failureBanner(wf.failure, run.run_id) : "");
  const actions = h("div", { class: "row-actions" },
    run.actions.cancel ? h("button", { type: "button", id: "run-cancel", class: "danger", text: "Cancel run", onclick: () => guarded(diag, async () => { await F.call("cancel", { run_id: id }); await runDetail(id); }) }) : "",
    retryDiag ? retryButton(id, retryDiag) : "",
    run.actions.reconcile ? h("button", { type: "button", id: "run-reconcile-jump", text: "Reconcile unknown effect (below)", onclick: () => document.getElementById("reconcile-panel")?.scrollIntoView() }) : "",
    h("button", { type: "button", text: "Refresh", onclick: () => runDetail(id) }));
  let steps;
  if (wf) {
    steps = h("table", { id: "run-steps" },
      h("thead", {}, h("tr", {}, ["#", "Step", "State", "Action", "Time", "Output / reason"].map((t) => h("th", { text: t })))),
      h("tbody", {}, wf.steps.map((s) => h("tr", { class: "step-" + s.state },
        h("td", { text: String(s.index) }), h("td", { text: s.id }), h("td", {}, chip(s.state.toUpperCase())), h("td", { text: s.action }), h("td", { text: dur(s) }),
        h("td", {}, s.state === "failed" ? h("span", { class: "danger", text: s.reason || "Failed; see native steps" }) : stepOutput(id, s))))));
  } else {
    steps = h("table", { id: "run-steps" },
      h("thead", {}, h("tr", {}, ["#", "Native step", "Result", "Output"].map((t) => h("th", { text: t })))),
      h("tbody", {}, run.steps.map((s) => h("tr", {}, h("td", { text: String(s.function_id) }), h("td", { text: s.function_name }), h("td", { text: s.error ? "error" : "ok" }), h("td", {}, h("code", { text: brief(s.output, 200) }))))));
  }
  const finalOutput = run.type === "workflow" && run.output ? run.output.output : run.output;
  const previousSteps = body.querySelector("#run-steps")?.parentElement;
  const sameRun = previousSteps?.dataset.runId === id;
  const stepsScroll = sameRun ? previousSteps.scrollLeft : 0;
  const stepsFocused = sameRun && document.activeElement === previousSteps;
  const stepsRegion = scrollTable("Run steps", steps);
  stepsRegion.dataset.runId = id;
  body.replaceChildren(
    h("p", {}, h("a", { href: "#/runs", text: "← All runs" })),
    header, actions, retryDiag, diag,
    h("h3", { text: "Steps" }), stepsRegion,
    h("h3", { text: "Final output" }), finalOutput === null || finalOutput === undefined ? h("p", { class: "empty", text: ACTIVE.includes(run.status) ? "Not finished yet." : "No published output." }) : pre(finalOutput, { role: "region", "aria-label": "Final output", tabindex: "0" }),
    h("details", {}, h("summary", { text: "Parameters" }), pre(run.params)),
    run.effects && run.effects.length ? h("details", { open: true }, h("summary", { text: "External effects" }), pre(run.effects)) : "",
    h("details", {}, h("summary", { text: "Audit log" }), h("ul", {}, (run.logs || []).map((l) => h("li", { text: `${when(l.created)} · ${l.actor} · ${l.action}${l.detail ? " · " + l.detail : ""}` })))),
    h("details", {}, h("summary", { text: "Native step checkpoints" }), pre(run.steps)));
  if (stepsFocused) stepsRegion.focus({ preventScroll: true });
  stepsRegion.scrollLeft = stepsScroll;
  if (F.runExtras) await F.runExtras(run, body, diag);
  if (ACTIVE.includes(run.status) && showingRun()) {
    pollTimer = setTimeout(() => { if (showingRun()) runDetail(id).catch((e) => alertBox(diag, e.message)); }, 1500);
  }
}
// A located failure names its step; an unlocatable one says so and points at
// the raw native checkpoints and audit instead of guessing a step.
function failureBanner(f, runId) {
  if (f.location === "unknown") return h("div", { class: "alert error", id: "run-failure", role: "alert" },
    h("strong", { text: "Failed: " }), f.reason,
    (f.unverified_refusals || []).length ? h("ul", {}, f.unverified_refusals.map((d) => h("li", { text: "Unverified refusal (not located): " + d }))) : "",
    h("p", { class: "hint", text: "See “Native step checkpoints” and “Audit log” below." }));
  return h("div", { class: "alert error", id: "run-failure", role: "alert" }, h("strong", { text: "Failed at step " + (f.step || "(workflow)") + ": " }), f.reason,
    f.contradicted ? h("p", { class: "hint", text: "Other native evidence conflicts with this stop; see “Native step checkpoints” and “Audit log” below, or agent-forge runs show " + runId + " --json." }) : "");
}
function stepOutput(runId, s) {
  if ((s.output === null || s.output === undefined) && s.note) return h("span", { class: "hint", text: s.note });
  if (s.output && s.output.summary === true && s.native_steps.length) {
    const box = h("span", {}, h("code", { text: brief(s.output.preview, 160) }), " ");
    box.append(h("button", { type: "button", text: "Load complete", onclick: async () => { const r = await F.call("inspect", { run_id: runId, step_id: s.native_steps[s.native_steps.length - 1] }); box.replaceChildren(pre(r.step.output)); } }));
    return box;
  }
  return h("code", { text: brief(s.output, 200) });
}

// ------------------------------------------------------------------ schedules
// Schedules are native DBOS schedules: DBOS fires each due tick and Forge admits
// one run per tick. The handoff (native firing) and the run's own outcome are
// shown separately; a definition is immutable, so "editing" creates a replacement.
const OUTCOME_CLASS = { succeeded: "SUCCESS", failed: "ERROR", cancelled: "CANCELLED", running: "PENDING", handoff_pending: "ENQUEUED", not_admitted: "ERROR" };
const outcomeChip = (o) => h("span", { class: "chip st-" + String(OUTCOME_CLASS[o] || "").toLowerCase(), text: o || "-" });
// Ticks keep their explicit offset: schedule-zone wall time for upcoming ticks, UTC for fired ones.
const iso = (t) => (t ? String(t).replace("T", " ") : "-");

let scheduleListRead = 0, scheduleDetailRead = 0, displayedScheduleQuery;
function retireScheduleForm(body) {
  const form = $("sc-create");
  if (form) body.replaceChildren(...[...body.children].filter((node) => node !== form));
}
async function scheduleList(query) {
  const requestedQuery = query.toString();
  const onList = () => {
    const route = parseHash();
    return route.view === "schedules" && !route.id && route.query.toString() === requestedQuery;
  };
  if (!onList()) return;
  const read = ++scheduleListRead, generation = routeGeneration;
  const isCurrent = () => generation === routeGeneration && read === scheduleListRead && onList();
  const body = $("sc-body");
  if (displayedScheduleQuery !== requestedQuery) retireScheduleForm(body);
  const tagFilter = query.get("tag") || "";
  const requestedWork = query.get("work");
  let nextSnapshot, list, stats, exactWork;
  try {
    nextSnapshot = await F.call("snapshot");
    if (!isCurrent()) return;
    list = tagFilter ? await F.call("schedules", { tag: tagFilter }) : nextSnapshot.schedules;
    if (!isCurrent()) return;
    stats = Object.fromEntries(((await F.call("schedule_stats")).schedules || []).map((x) => [x.schedule_name, x]));
    if (!isCurrent()) return;
    if (requestedWork && !nextSnapshot.work.some((w) => w.id === requestedWork)) {
      // The snapshot is capped; a workflow deep link can reach an older ID.
      try { exactWork = await F.call("work", { work_id: requestedWork }); }
      catch { /* Keep the schedule list usable and show the unavailable selection in the form. */ }
    }
  } catch (error) {
    if (isCurrent()) {
      const message = error.message || String(error);
      alertBox($("sc-alert"), message);
      F.status("Failed / state may be stale: " + message);
    }
    return;
  }
  if (!isCurrent()) return;
  const allTags = [...new Set(nextSnapshot.schedules.flatMap((s) => s.tags || []))].sort();
  const rows = list.map((s) => h("tr", { "data-schedule": s.schedule_name },
    h("td", {}, h("a", { href: "#/schedules/" + encodeURIComponent(s.schedule_name), text: s.schedule_name })),
    h("td", {}, chip(s.status)),
    h("td", {}, h("a", { href: "#/workflows/" + encodeURIComponent(s.context.work_id), text: s.context.work_id })),
    h("td", {}, h("code", { text: s.schedule }), " ", h("small", { text: s.cron_timezone || "UTC" })),
    h("td", { text: iso(s.last_fired_at) }),
    h("td", { text: s.next_run ? iso(s.next_run) : "not firing" }),
    h("td", {}, (s.tags || []).length ? (s.tags || []).map((t) => h("a", { class: "chip", href: "#/schedules?tag=" + encodeURIComponent(t), text: t })) : "-"),
    h("td", { class: "stat-ok", text: String(stats[s.schedule_name]?.succeeded ?? 0) }),
    h("td", { class: "stat-bad", text: String(stats[s.schedule_name]?.failed ?? 0) }),
    h("td", { text: String((stats[s.schedule_name]?.cancelled ?? 0) + (stats[s.schedule_name]?.running ?? 0)) }),
    h("td", {}, stats[s.schedule_name]?.last_run_id ? h("a", { href: "#/runs/" + encodeURIComponent(stats[s.schedule_name].last_run_id), text: stats[s.schedule_name].last_outcome || "run" }) : "-")));
  snapshot = nextSnapshot;
  body.replaceChildren(
    h("h2", { text: "Schedules" }),
    h("p", { text: "DBOS fires each due tick on the schedule's IANA wall clock and Forge admits one run per tick. Missed ticks (while paused or while no service runs) are skipped, not caught up." }),
    h("div", { class: "row-actions", id: "sc-filter" }, h("label", {}, "Filter by tag ", h("select", { id: "sc-tag-filter", onchange: (e) => go("#/schedules" + (e.target.value ? "?tag=" + encodeURIComponent(e.target.value) : "")) },
      [h("option", { value: "", text: "all schedules" }), ...allTags.map((t) => h("option", { value: t, text: t, selected: t === tagFilter }))])),
      tagFilter ? h("span", { class: "hint", text: list.length + " schedule(s) tagged " + tagFilter }) : ""),
    list.length ? h("p", { class: "hint", text: "Succeeded, failed and other counts cover each schedule's most recent occurrences (up to 100)." }) : "",
    list.length ? h("table", { id: "sc-table" }, h("thead", {}, h("tr", {}, ["Schedule", "Status", "Work", "Cron · zone", "Last fired", "Next due", "Tags", "Succeeded", "Failed", "Other", "Last run"].map((t) => h("th", { text: t })))), h("tbody", {}, rows))
      : h("p", { class: "empty", text: "No schedules yet. Create one below." }),
    scheduleForm(query, exactWork, onList));
  displayedScheduleQuery = requestedQuery;
}

function scheduleForm(query, exactWork, onList) {
  const diag = h("div", { id: "sc-form-diag" });
  const previewBox = h("div", { id: "sc-preview" });
  const name = h("input", { id: "sc-name", required: true, pattern: "[a-zA-Z0-9][a-zA-Z0-9_.\\-]{0,79}", placeholder: "e.g. hourly-digest", value: query.get("name") || "" });
  const requestedWork = query.get("work");
  const choices = snapshot.work.filter((w) => w.enabled);
  if (exactWork?.id === requestedWork && exactWork?.enabled) choices.push(exactWork);
  const selected = choices.find((w) => w.id === requestedWork);
  const missing = requestedWork && !selected;
  const work = h("select", { id: "sc-work", required: true },
    missing || !choices.length ? h("option", { value: "", disabled: true, text: "Choose enabled work" }) : null,
    choices.map((w) => h("option", { value: w.id, text: w.id + " (" + w.definition.type + ")" })));
  if (requestedWork) work.value = selected?.id || "";
  const cron = h("input", { id: "sc-cron", required: true, value: query.get("cron") || "0 * * * *", size: 22 });
  const tz = h("input", { id: "sc-tz", required: true, value: query.get("tz") || Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC", size: 22 });
  const params = h("textarea", { id: "sc-params", rows: 3, text: query.get("params") || "{}" });
  const tags = h("input", { id: "sc-tags", size: 30, placeholder: "comma-separated, optional" });
  const check = () => guarded(diag, async () => {
    const p = await F.call("schedule_preview", { cron: cron.value, tz: tz.value });
    previewBox.replaceChildren(h("div", { class: "alert ok", role: "status" },
      h("strong", { text: "Valid: " }), h("code", { text: p.schedule }), " in " + p.cron_timezone + ". Next due ticks:",
      h("ul", {}, p.upcoming.map((t) => h("li", { text: t.local + "  (" + t.utc + " UTC)" }))),
      h("small", { text: p.catchup })));
  });
  const create = () => guarded(diag, async () => {
    if (!onList() || !name.reportValidity() || !work.reportValidity()) return;
    previewBox.replaceChildren();
    let parsed;
    try { parsed = JSON.parse(params.value || "{}"); } catch (e) { throw Error("Invocation parameters must be valid JSON: " + e.message); }
    const local = name.value;
    const list = await F.call("schedule", { name: local, work_id: work.value, cron: cron.value, tz: tz.value, params: parsed, tags: tags.value.split(",").map((t) => t.trim()).filter(Boolean) });
    // The canonical identity is exactly actor + "." + submitted local name; never suffix-match
    // (operator.a.digest must not stand in for a newly created operator.digest).
    const made = list.find((s) => s.schedule_name === s.context.actor + "." + local);
    if (onList()) go("#/schedules/" + encodeURIComponent(made ? made.schedule_name : ""));
  });
  return h("section", { class: "card", id: "sc-create" },
    h("h3", { text: query.get("replace") ? "Create replacement for " + query.get("replace") : "New schedule" }),
    query.get("replace") ? h("p", { class: "alert info", text: "This creates a new schedule under a new name (a schedule's work is fixed); the original keeps running until you pause or delete it. Cron, zone, params and tags can be edited in place on the schedule's own page." }) : "",
    h("div", { class: "row-actions" }, h("label", {}, "Name", name), h("label", {}, "Work", work)),
    missing ? h("p", { class: "hint", text: `Cannot select ${requestedWork}: it is unavailable or disabled. Choose enabled work.` }) : null,
    h("div", { class: "row-actions" },
      h("label", {}, "Cron (5 fields minute-first, or 6 seconds-first)", cron),
      h("label", {}, "IANA time zone", tz)),
    h("label", { class: "wide" }, "Invocation parameters JSON (workflow, agent or harness work only)", params),
    h("div", { class: "row-actions" }, h("label", {}, "Tags", tags)),
    h("div", { class: "row-actions" },
      h("button", { type: "button", id: "sc-check", text: "Check timing", onclick: check }),
      h("button", { type: "button", id: "sc-submit", class: "primary", text: "Create schedule", onclick: create })),
    previewBox, diag);
}

function scheduleEditCard(s, name, diag, reload) {
  if (!s.actions.edit) return "";
  const cron = h("input", { id: "sc-edit-cron", size: 22, value: s.schedule });
  const tz = h("input", { id: "sc-edit-tz", size: 22, value: s.cron_timezone || "UTC" });
  const params = h("textarea", { id: "sc-edit-params", rows: 3, text: s.params ? JSON.stringify(s.params, null, 1) : "{}" });
  const tags = h("input", { id: "sc-edit-tags", size: 30, value: (s.tags || []).join(", ") });
  const enabled = h("input", { id: "sc-edit-enabled", type: "checkbox" });
  enabled.checked = s.status === "ACTIVE";
  const preview = h("div", { id: "sc-edit-preview" });
  const check = () => guarded(diag, async () => {
    const p = await F.call("schedule_preview", { cron: cron.value, tz: tz.value, count: 6 });
    preview.replaceChildren(h("div", { class: "alert ok", role: "status" }, h("strong", { text: "Valid: " }), h("code", { text: p.schedule }), " in " + p.cron_timezone + ". Next ticks:",
      h("ul", {}, p.upcoming.map((t) => h("li", { text: t.local + "  (" + t.utc + " UTC)" })))));
  });
  const save = () => guarded(diag, async () => {
    let parsed;
    try { parsed = JSON.parse(params.value || "{}"); } catch (e) { throw Error("Parameters must be valid JSON: " + e.message); }
    const r = await F.call("schedule_update", { name, cron: cron.value, tz: tz.value, params: parsed, enabled: enabled.checked, tags: tags.value.split(",").map((t) => t.trim()).filter(Boolean) });
    clearTimeout(pollTimer);
    await reload(name, r.changed.length ? "Saved (" + r.changed.join(", ") + "). History kept; no tick fires twice." : "Nothing changed.");
  });
  return h("section", { class: "card", id: "sc-edit" }, h("h3", { text: "Edit in place" }),
    h("p", { class: "hint", text: "Changes cron, zone, parameters, enabled state and tags without a new name. The work is fixed. Previous revisions and the occurrence history are kept; a tick that already fired never fires again." }),
    h("div", { class: "row-actions" }, h("label", {}, "Cron", cron), h("label", {}, "IANA time zone", tz), h("label", {}, enabled, " Enabled")),
    h("label", { class: "wide" }, "Parameters JSON", params),
    h("div", { class: "row-actions" }, h("label", {}, "Tags", tags)),
    h("div", { class: "row-actions" }, h("button", { type: "button", id: "sc-edit-check", text: "Check timing", onclick: check }), h("button", { type: "button", id: "sc-edit-save", class: "primary", text: "Save changes", onclick: save })),
    preview);
}

async function scheduleDetail(name, notice) {
  const onDetail = () => {
    const route = parseHash();
    return route.view === "schedules" && route.id === name;
  };
  if (!onDetail()) return;
  const read = ++scheduleDetailRead, generation = routeGeneration;
  const isCurrent = () => generation === routeGeneration && read === scheduleDetailRead && onDetail();
  const body = $("sc-body");
  retireScheduleForm(body);
  const diag = h("div", { id: "sc-diag" });
  if (notice) alertBox(diag, notice, "ok");
  let s;
  try { s = await F.call("schedule_show", { name, limit: 20 }); }
  catch (error) { if (isCurrent()) throw error; return; }
  if (!isCurrent()) return;
  const act = (action) => guarded(diag, async () => {
    if (action === "delete" && !confirm("Delete schedule " + name + "? Admitted runs continue; history stays readable.")) return;
    await F.call("schedule_action", { name, action });
    clearTimeout(pollTimer);
    await scheduleDetail(name, { pause: "Paused: no further ticks are admitted; ticks missed while paused are skipped.", resume: "Resumed: fires from the next due tick.", delete: "Deleted natively; retained definition and history remain readable." }[action]);
  });
  // Local name = canonical name minus the exact "<actor>." prefix (actors may contain dots).
  const prefix = s.context.actor + ".";
  const local = name.startsWith(prefix) ? name.slice(prefix.length) : name;
  const replaceQuery = new URLSearchParams({ replace: name, name: local + "-v2", work: s.context.work_id, cron: s.schedule, tz: s.cron_timezone || "UTC", params: JSON.stringify(s.params || {}) });
  const counts = Object.entries(s.outcome_counts || {});
  // Explicit one-shot manual run: only on click, disabled while in flight, never
  // retried. Each click is a new native trigger and a new run.
  const runOnce = h("button", { type: "button", id: "sc-run-once", class: "primary", text: "Run once now",
    title: "Start a new manual run of this schedule's frozen work and params now. Not a due tick; the schedule's status, cron and last-fired tick do not change." });
  runOnce.onclick = async () => {
    if (runOnce.disabled) return;
    runOnce.disabled = true; runOnce.textContent = "Starting…";
    clearTimeout(pollTimer);
    let r;
    try { r = await F.call("schedule_trigger", { name }); }
    catch (e) {
      alertBox(diag, e.message + " The manual run was not retried. Check the history below for a manual run before running again.");
      runOnce.disabled = false; runOnce.textContent = "Run once now";
      return;
    }
    const msg = r.run_id ? `Manual run started (trigger ${r.trigger_id}); run ${r.run_id}. Schedule stays ${r.schedule_status}.` : `Manual run enqueued (trigger ${r.trigger_id}) but not admitted yet. Do not run again; watch the history.`;
    await scheduleDetail(name, msg);
  };
  const edit = scheduleEditCard(s, name, diag, scheduleDetail);
  const occ = s.occurrences.length ? h("table", { id: "sc-occurrences" },
    h("thead", {}, h("tr", {}, ["Time", "Kind", "Handoff", "Outcome", "Run", "Result"].map((t) => h("th", { text: t })))),
    h("tbody", {}, s.occurrences.map((o) => h("tr", { "data-outcome": o.outcome, "data-kind": o.kind, "data-occurrence": o.occurrence_id },
      h("td", { text: iso(o.scheduled_time), title: "Browser local: " + new Date(o.scheduled_time).toLocaleString() }),
      h("td", {}, h("span", { class: "chip " + (o.kind === "manual" ? "st-admitted" : ""), text: o.kind === "manual" ? "Manual run" : "Due tick" })),
      h("td", {}, chip(o.handoff)),
      h("td", {}, outcomeChip(o.outcome)),
      h("td", {}, o.run_id ? h("a", { href: "#/runs/" + encodeURIComponent(o.run_id), text: o.run_id.slice(0, 18) + "…", title: o.run_id }) : h("span", { class: "hint", text: "none" })),
      h("td", {}, o.outcome === "failed" ? h("span", { class: "danger", text: o.failure && o.failure.reason ? "failed at " + (o.failure.step || "workflow") + ": " + brief(o.failure.reason, 80) : brief(o.run && (o.run.error || o.run.diagnostic), 80) })
        : o.outcome === "not_admitted" ? h("span", { class: "danger", text: o.handoff_error || "Handoff failed before admission" })
        : h("code", { text: brief(o.run && o.run.output && o.run.output.output !== undefined ? o.run.output.output : o.run && o.run.output, 120) }))))))
    : h("p", { class: "empty", id: "sc-no-occurrences", text: s.status === "ACTIVE" ? "No due occurrences yet. This page refreshes while the schedule is active." : "No due occurrences." });
  body.replaceChildren(
    h("p", {}, h("a", { href: "#/schedules", text: "← All schedules" })),
    h("div", { class: "card", id: "sc-head" },
      h("h2", {}, "Schedule ", h("code", { text: s.schedule_name })),
      h("p", {}, chip(s.status), " runs ", h("a", { href: "#/workflows/" + encodeURIComponent(s.context.work_id), text: s.context.work_id }), " · ", h("code", { text: s.schedule }), " in " + (s.cron_timezone || "UTC")),
      h("table", { id: "sc-facts" }, h("tbody", {},
        [["Last fired", iso(s.last_fired_at)],
         ["Next due", s.upcoming.length ? s.upcoming.map((t) => iso(t.local)).join(" · ") : "not firing while " + s.status],
         ["Catch-up", s.catchup],
         ["Timing", s.authority],
         ["Parameters", s.params ? JSON.stringify(s.params) : "none"],
         ["Tags", (s.tags || []).length ? s.tags.join(", ") : "none"],
         ["Revision", s.revision ? s.revision + " (edited in place; history kept)" : "original"],
         ["Outcomes (recent)", counts.length ? counts.map(([k, v]) => k + " " + v).join(" · ") : "none yet"]].map(([k, v]) => h("tr", {}, h("th", { text: k }), h("td", { text: v })))))),
    h("div", { class: "row-actions" },
      s.actions.pause ? h("button", { type: "button", id: "sc-pause", text: "Pause", onclick: () => act("pause") }) : "",
      s.actions.resume ? h("button", { type: "button", id: "sc-resume", class: "primary", text: "Resume", onclick: () => act("resume") }) : "",
      s.actions.delete ? h("button", { type: "button", id: "sc-delete", class: "danger", text: "Delete", onclick: () => act("delete") }) : "",
      s.status !== "DELETED" && !s.pending_edit ? runOnce : "",
      h("a", { href: "#/schedules?" + replaceQuery, id: "sc-replace", text: "Create replacement…" }),
      h("button", { type: "button", text: "Refresh", onclick: () => scheduleDetail(name) })),
    h("p", { class: "hint", text: s.replacement }),
    s.pending_edit ? h("div", { class: "alert warn", id: "sc-pending-edit", role: "status" },
      s.pending_edit.native_present
        ? ["Use the pending cron, time zone and parameters from Canonical JSON with ",
          h("code", { text: "agent-forge schedules edit" }), " or ", h("code", { text: "forge_schedule_update" }),
          ". Use --clear-params or null to clear parameters. Omit enabled and tags. The current pause state is preserved."]
        : "The native schedule is missing. Resubmission requires the original native schedule. To schedule future work, use a different name.") : "",
    diag,
    edit,
    h("h3", { text: "Occurrence history (newest first)" }),
    h("p", { class: "hint", text: "Handoff is the native DBOS firing that admits the run; Outcome is the admitted run's own result. Kind separates a due tick from an explicit manual run." }),
    occ,
    h("details", {}, h("summary", { text: "Canonical JSON" }), pre(s)));
  // Refresh while the schedule can fire, or while any occurrence (e.g. a manual run
  // of a paused schedule) has not finished. A refresh only reads; it never triggers.
  const unfinished = s.occurrences.some((o) => ["running", "handoff_pending"].includes(o.outcome));
  if (s.status === "ACTIVE" || unfinished) pollTimer = setTimeout(() => { if (parseHash().id === name) scheduleDetail(name, notice).catch((e) => alertBox(diag, e.message)); }, unfinished ? 1500 : 5000);
}

// ------------------------------------------------------------------ queues
// Native DBOS queues: stored limits, home-local counts, the caller's own waiting
// and running work with recorded reasons, and recent results. No position or
// start estimate is shown because DBOS does not expose one.
const stamp = (ms) => (typeof ms === "number" ? new Date(ms).toISOString().replace("T", " ").replace(/\.\d+Z$/, " UTC") : "-");
const secs = (ms) => (typeof ms === "number" ? (ms / 1000).toFixed(1) + "s" : "-");
const REASONS = { queue_capacity: "Queue capacity", native_delay: "Native delay", rate_limited: "Rate limited", not_determined: "Not determined" };
function policyText(q) {
  const p = q.policy;
  if (!p) return "No stored native policy (DBOS built-in queue)";
  const parts = [p.concurrency != null ? `concurrency ${p.concurrency}` : "no global concurrency limit"];
  if (p.worker_concurrency != null) parts.push(`per-executor ${p.worker_concurrency}`);
  if (p.partitioned) parts.push(`partitioned: ${p.partition_concurrency ?? "unlimited"} per partition`);
  parts.push(p.rate_limit ? `rate limit ${p.rate_limit.limit} per ${p.rate_limit.period_ms} ms` : "no rate limit");
  parts.push(`polls every ${p.polling_interval_ms} ms`);
  return parts.join(" · ");
}
function memberLink(m) {
  if (m.kind !== "run") return h("span", { text: "Tick handoff of " + m.schedule_name, title: m.handoff_id });
  return h("a", { href: "#/runs/" + encodeURIComponent(m.run_id), text: m.run_id.slice(0, 18) + "…", title: m.run_id });
}
function doing(a) {
  if (!a) return "Schedule tick handoff";
  if (a.kind === "durable_sleep") return "Durable sleep until " + stamp(a.until) + " (holds its slot)";
  if (a.kind === "executing") return `Executing · last checkpoint ${a.last_checkpoint.step_id} ${a.last_checkpoint.name}`;
  return "Dequeued; no checkpoint yet";
}
function queueCard(q, diag, reload) {
  const members = q.members || [];
  const running = members.filter((m) => m.state === "running");
  const waiting = members.filter((m) => m.state !== "running");
  const cancel = (m) => m.kind === "run" && m.actions.cancel ? h("button", { type: "button", class: "danger q-cancel", "data-run": m.run_id, text: "Cancel", title: m.cancel_note,
    onclick: () => guarded(diag, async () => {
      if (!confirm(`Cancel run ${m.run_id}?\n\n${m.cancel_note}`)) return;
      const r = await F.call("cancel", { run_id: m.run_id });
      alertBox(diag, `Cancelled ${m.run_id}: now ${r.status}.`, "ok");
      await reload(false);
    }) }) : "";
  const cap = q.capacity;
  const out = h("div", { class: "card q-card", id: "q-" + q.key, "data-saturated": cap.saturated === null ? "unknown" : String(!!cap.saturated) },
    h("h3", {}, "Queue ", h("code", { text: q.key }), " ", cap.saturated === true ? chip("SATURATED") : "", h("small", { text: "  " + q.name })),
    h("p", { class: "q-summary", text: q.summary }),
    h("table", { class: "q-facts" }, h("tbody", {},
      [["Slots", cap.slots == null ? `${cap.running} running · no global limit` : cap.free == null ? `at least ${cap.running} of ${cap.slots} running · free slots unknown: native scan incomplete (${cap.kind})` : `${q.counts.complete ? "" : "at least "}${cap.running} of ${cap.slots} running · ${cap.free} free (${cap.kind})`],
       ["Waiting", `${q.counts.waiting} waiting · ${q.counts.delayed} delayed` + (q.counts.complete ? "" : ` (at least; first ${q.counts.scan_limit} records read)`)],
       ["Outside your scope", `${q.counts.outside_your_scope.running} running · ${q.counts.outside_your_scope.waiting} waiting (counted, not identified)`],
       ["Native policy", policyText(q)],
       ["Home policy", q.home_policy ? JSON.stringify(q.home_policy) : "DBOS-owned"],
       ["Role", q.role]].map(([k, v]) => h("tr", {}, h("th", { text: k }), h("td", { text: v }))))));
  if (q.key === "internal") return out;
  out.append(h("h4", { text: "Running — yours" }), running.length ? h("table", { class: "q-running" },
    h("thead", {}, h("tr", {}, ["Run", "Work", "Params", "Started", "Doing", ""].map((t) => h("th", { text: t })))),
    h("tbody", {}, running.map((m) => h("tr", { "data-run": m.run_id || m.handoff_id },
      h("td", {}, memberLink(m)), h("td", { text: m.work_id }), h("td", {}, h("code", { text: brief(m.params, 60) })), h("td", { text: stamp(m.started_at) }), h("td", { text: doing(m.activity) }), h("td", {}, cancel(m))))))
    : h("p", { class: "empty q-empty-running", text: emptyText(q, "running") }));
  out.append(h("h4", { text: "Waiting — yours" }), waiting.length ? h("table", { class: "q-waiting" },
    h("thead", {}, h("tr", {}, ["Run", "Work", "Params", "Waiting for", "Reason", "Why", ""].map((t) => h("th", { text: t })))),
    h("tbody", {}, waiting.map((m) => h("tr", { "data-run": m.run_id || m.handoff_id, "data-reason": m.wait.reason },
      h("td", {}, memberLink(m)), h("td", { text: m.work_id }), h("td", {}, h("code", { text: brief(m.params, 60) })), h("td", { text: secs(m.wait.waiting_ms) }),
      h("td", {}, h("span", { class: "chip st-" + (m.wait.reason === "not_determined" ? "cancelled" : "enqueued"), text: REASONS[m.wait.reason] || m.wait.reason })),
      h("td", { class: "q-why", text: m.wait.explanation }), h("td", {}, cancel(m))))))
    : h("p", { class: "empty q-empty-waiting", text: emptyText(q, "waiting") }));
  if (waiting.length) out.append(h("p", { class: "hint", text: "Position and start time are not shown: DBOS does not expose a queue position or start estimate. A waiting run cancelled before dequeue never starts." }));
  const listing = q.members_listing || {};
  if (listing.identities_listed && listing.complete !== true) out.append(h("div", { class: "alert warn q-incomplete", role: "status" },
    h("strong", { text: `Your listed work is incomplete: ${listing.shown} shown of ${listing.found_in_scan} found${listing.native_scan_complete ? "" : " in an incomplete native scan"}. ` }),
    listing.note || "", " ", h("a", { href: "#/runs", text: "Find your running, waiting and delayed runs in Runs" })));
  if (q.key === "work") {
    const win = q.recent_window || {};
    out.append(h("h4", { text: "Recent results — yours" + (win.complete === true ? "" : " (sampled)") }), (q.recent || []).length ? h("table", { class: "q-recent" },
      h("thead", {}, h("tr", {}, ["Run", "Work", "Status", "Waited", "Ran", "Result"].map((t) => h("th", { text: t })))),
      h("tbody", {}, q.recent.map((r) => h("tr", { "data-run": r.run_id },
        h("td", {}, h("a", { href: "#/runs/" + encodeURIComponent(r.run_id), text: r.run_id.slice(0, 18) + "…", title: r.run_id })),
        h("td", { text: r.work_id }), h("td", {}, chip(r.status)), h("td", { text: secs(r.waited_ms) }), h("td", { text: secs(r.ran_ms) }),
        h("td", {}, h("code", { text: brief(r.output && r.output.output !== undefined ? r.output.output : r.output, 120) }))))))
      : h("p", { class: "empty q-recent-empty", text: win.complete === true ? "No finished runs of yours recorded on this queue." : win.unavailable > 0 ? "None of yours can be shown right now; some exist but could not be read." : "None of yours in the sampled window." }),
      win.complete === true ? "" : h("p", { class: "hint q-recent-sampled" }, win.note || "Sampled.", " ", h("a", { href: "#/runs", text: "Open Runs for full history" })),
      h("p", { class: "hint", text: "Waited = created until DBOS dequeued it; Ran = dequeued until finished. Cancelled runs leave the queue and are listed in Runs." }));
  }
  return out;
}
// Absence is stated only when the listing for that state is proven complete.
// The Waiting section holds both waiting (ENQUEUED) and delayed members, so it
// is proven empty only when both states are complete.
function emptyText(q, state) {
  const l = q.members_listing || {};
  const states = state === "waiting" ? ["waiting", "delayed"] : [state];
  if (states.every((s) => ((l.by_state || {})[s] || {}).complete === true)) return state === "waiting" ? "None of your work is waiting or delayed here." : `None of your work is ${state} here.`;
  if (state === "waiting") state = "waiting or delayed";
  if (l.native_scan_complete === false) return `None of yours found among the records read; the native scan was incomplete, so your ${state} work may exist (see the notice and Runs).`;
  return `None listed; some of your work was omitted by the member limit, so ${state} work of yours may exist.`;
}
async function queueView(only, query) {
  const limit = query && query.get("limit") ? Number(query.get("limit")) : undefined;
  const body = $("q-body");
  const diag = h("div", { id: "q-diag" });
  const reload = async (keepDiag = true) => { clearTimeout(pollTimer); const old = $("q-diag"); await render(keepDiag && old ? old : diag); };
  async function render(diagEl) {
    const v = await F.call("queues", { ...(only ? { queue: only } : {}), ...(limit ? { limit } : {}) });
    const busy = v.queues.some((q) => q.key !== "internal" && (q.capacity.running > 0 || q.counts.waiting > 0 || q.counts.delayed > 0)) || v.admitted_not_on_queue.yours.length > 0;
    const pending = v.admitted_not_on_queue.yours;
    body.replaceChildren(
      h("h2", { text: "Queues" }),
      h("p", { text: "DBOS owns these queues: their limits, which workflow starts next and when. Counts cover this whole home; only your own work is identified." }),
      h("p", { class: "hint", id: "q-observed", text: "Observed " + stamp(v.observed_at) + (busy ? " · refreshing every 2 s while work is active" : "") }),
      h("div", { class: "row-actions" }, h("button", { type: "button", text: "Refresh", onclick: () => reload() }),
        only ? h("a", { href: "#/queues", text: "All queues" }) : h("a", { href: "#/queues/work", text: "Work queue only" })),
      diagEl,
      pending.length ? h("div", { class: "alert info", id: "q-unsubmitted" }, h("strong", { text: "Admitted, not yet on the native queue: " }), pending.map((p) => p.run_id).join(", "), h("br"), pending[0].explanation) : "",
      ...v.queues.map((q) => queueCard(q, diagEl, reload)),
      h("details", {}, h("summary", { text: "What these numbers mean" }), h("ul", {}, Object.values(v.observation).map((t) => h("li", { text: t })))),
      h("details", {}, h("summary", { text: "Canonical JSON" }), pre(v)));
    if (busy) pollTimer = setTimeout(() => { if (parseHash().view === "queues") reload().catch((e) => alertBox($("q-diag") || diag, e.message)); }, 2000);
  }
  await render(diag);
}

route();

// Shared helpers for the views in forge-views.js (loaded after this module).
F.ui = { h, when, dur, chip, brief, pre, newKey, alertBox, guarded, go, parseHash, registerView, route, scrollTable, STATUSES, ACTIVE, stamp, iso, snapshot: () => snapshot, routeGeneration: () => routeGeneration, setPoll: (fn, ms) => { clearTimeout(pollTimer); pollTimer = setTimeout(fn, ms); } };

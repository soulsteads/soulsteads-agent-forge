"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const test = require("node:test");
const vm = require("node:vm");

function section(source, start, end) {
  const from = source.indexOf(start), to = source.indexOf(end, from + start.length);
  assert(from >= 0 && to > from, `missing source section ${start}`);
  return source.slice(from, to);
}
const root = require("node:path").resolve(__dirname, "..");
const ui = fs.readFileSync(process.env.FORGE_SCHEDULES_UI || root + "/assets/forge-ui.js", "utf8");
const core = fs.readFileSync(root + "/assets/forge-core.js", "utf8");
const source = [
  section(ui, "function h(", "const when = "),
  section(ui, "const chip = ", "function newKey("),
  section(ui, "function alertBox(", "// ------------------------------------------------------------------ routing"),
  section(ui, "const ALERT_IDS = ", 'registerView("workflows"'),
  section(ui, "const OUTCOME_CLASS", "// ------------------------------------------------------------------ queues"),
  ui.match(/^registerView\("schedules",.*$/m)[0],
  ui.match(/^F\.onConnect\.push.*$/m)[0],
  section(core, "function text(", "async function call("),
  section(core, "async function act(", "async function inspect("),
  section(core, "async function refresh(", "async function pluginFetch("),
  core.match(/\$\('refresh'\)\.onclick=\(\)=>act\(async\(\)=>\{\}\);/)[0],
].join("\n");

class Element {
  constructor(tag, textContent = "") {
    this.localName = tag;
    this.textContent = textContent;
    this.attributes = {};
    this.children = [];
    this.listeners = {};
  }
  setAttribute(key, value) { this.attributes[key] = String(value); }
  addEventListener(event, listener) { this.listeners[event] = listener; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  findAll(predicate) {
    return [
      ...(predicate(this) ? [this] : []),
      ...this.children.flatMap(child => child instanceof Element ? child.findAll(predicate) : []),
    ];
  }
  set value(value) { this.inputValue = value; }
  get value() {
    if (this.inputValue !== undefined) return this.inputValue;
    if (this.localName === "select") {
      const option = this.children.find(node => Object.hasOwn(node.attributes, "selected")) ?? this.children[0];
      return option?.value ?? "";
    }
    return this.attributes.value ?? "";
  }
}
const text = (node) => node.textContent + node.children.map(text).join("");

function offlinePage() {
  const roots = new Map(["sc-body", "sc-alert", "refresh", "status", "type-schemas", "runners",
    "runner-matrix", "works", "schedule-list", "run-list"].map(id => [id, new Element("div")]));
  const body = roots.get("sc-body"), diag = roots.get("sc-alert"), status = roots.get("status");
  const globals = [], calls = [], views = new Map(), typeSelect = new Element("select");
  const location = { hash: "#/schedules" };
  const $ = id => roots.get(id) ?? body.findAll(node => node.attributes.id === id)[0];
  const read = (method, params) => {
    assert(["snapshot", "schedules", "schedule_stats", "schedule_show"].includes(method),
      "only inert schedule reads are permitted");
    return new ContextPromise((resolve, reject) => calls.push({
      method, params: params === undefined ? undefined : JSON.parse(JSON.stringify(params)), resolve, reject,
    }));
  };
  const F = { call: read, connected: () => true, onConnect: [],
    status(message) { globals.push(message); status.textContent = message; } };
  const context = vm.createContext({
    document: {
      createElement: tag => new Element(tag),
      createTextNode: value => new Element("#text", value),
      querySelector(selector) { assert.equal(selector, "#register select[name=type]"); return typeSelect; },
    },
    Node: Element, URLSearchParams, location, views, $, F, window: { forge: F },
    call: read, selected: null, snapshot: null, pollTimer: null, show() {}, cleanupPlugin() {}, renderPluginNav() {},
    clearTimeout() {}, setTimeout() { assert.fail("the inert detail is paused; no timer is expected"); },
    fetch() { assert.fail("network access is unavailable"); },
    registerView: (view, loader) => views.set(view, loader),
  });
  const ContextPromise = vm.runInContext("Promise", context);
  vm.runInContext(source, context);
  const links = node => node.findAll(n => n.localName === "a").map(n => n.attributes.href);
  return {
    calls, globals,
    navigate(hash) { location.hash = hash; return context.route(); },
    refresh: () => $("refresh").onclick(),
    diagnostic: () => diag.children[0]?.textContent || "",
    diagnosticRole: () => diag.children[0]?.attributes.role,
    shared: () => context.snapshot?.health.time,
    list() {
      const row = $("sc-table").findAll(n => Object.hasOwn(n.attributes, "data-schedule"))[0];
      return { filter: $("sc-tag-filter").value, identity: row.attributes["data-schedule"],
        cells: row.children.map(text), links: links(row), form: $("sc-name") ? {
          name: $("sc-name").value, work: $("sc-work").value,
          choices: $("sc-work").children.map(n => n.value),
          cron: $("sc-cron").value, timezone: $("sc-tz").value, params: $("sc-params").textContent,
        } : null };
    },
    detail: () => ({ head: text($("sc-head")), history: text($("sc-occurrences")), links: links($("sc-occurrences")) }),
    detailMounted: () => Boolean($("sc-head")),
  };
}
const afterReads = () => new Promise(resolve => setImmediate(resolve));

function sample(label, count) {
  const work = label + ".echo", name = "operator.team." + label;
  const schedule = { schedule_name: name, status: "PAUSED", schedule: "0 30 12 * * *",
    cron_timezone: "America/Chicago", tags: [label], context: { actor: "operator.team", work_id: work } };
  return {
    snapshot: { types: {}, runners: { profiles: [], matrix: [] }, health: { time: label },
      runs: [], schedules: [schedule], work: [{ id: work, enabled: true, definition: { type: "echo" } }] },
    list: [schedule],
    stats: { schedules: [{ schedule_name: name, succeeded: count, failed: 2, cancelled: 3, running: 4,
      last_run_id: label + "/final", last_outcome: "succeeded" }] },
    detail: { ...schedule, actions: {}, upcoming: [], occurrences: [{
      kind: "due", outcome: "succeeded", occurrence_id: label + "-occurrence",
      handoff: "admitted", scheduled_time: "2026-11-19T12:30:00Z",
      run_id: label + "/final", run: { output: { output: "retained result" } },
    }] },
  };
}
const initial = sample("initial", 1), old = sample("old", 5), latest = sample("latest", 11);

function answer(f, index, method, data, params) {
  assert.equal(f.calls[index].method, method);
  assert.deepEqual(f.calls[index].params, params);
  f.calls[index].resolve(data);
}
async function completeList(f, start, data, tag) {
  answer(f, start, "snapshot", data.snapshot);
  await afterReads();
  if (tag) {
    answer(f, ++start, "schedules", data.list, { tag });
    await afterReads();
  }
  answer(f, ++start, "schedule_stats", data.stats);
}
async function mounted() {
  const f = offlinePage(), action = f.navigate("#/schedules");
  await completeList(f, 0, initial);
  await action;
  return f;
}
function expectLatest(f, tag = "", form = { name: "", work: "latest.echo" }) {
  const values = f.list();
  assert.equal(values.filter, tag);
  assert.equal(values.identity, "operator.team.latest");
  assert.deepEqual(values.cells.slice(-4), ["11", "2", "7", "succeeded"]);
  assert.deepEqual(values.links, ["#/schedules/operator.team.latest", "#/workflows/latest.echo",
    "#/schedules?tag=latest", "#/runs/latest%2Ffinal"]);
  assert.deepEqual(values.form, { ...form, choices: ["latest.echo"], cron: "0 * * * *",
    timezone: "America/Chicago", params: "{}" });
  assert.equal(f.shared(), "latest");
}
const latestQuery = "#/schedules?tag=latest&name=latest-copy&work=latest.echo&tz=America%2FChicago";

test("ordinary global Refresh invokes the registered list and publishes exact latest history and form", async () => {
  const f = await mounted(), route = f.navigate(latestQuery);
  await completeList(f, 2, initial, "latest");
  await route;
  const refresh = f.refresh();
  await afterReads();
  answer(f, 5, "snapshot", latest.snapshot);
  await afterReads();
  await completeList(f, 6, latest, "latest");
  await refresh;
  expectLatest(f, "latest", { name: "latest-copy", work: "latest.echo" });
  assert.equal(f.diagnostic(), "");
});

test("old tag read cannot replace the latest filter, form, shared data or history", async () => {
  const f = await mounted(), older = f.navigate("#/schedules?tag=old&name=old-copy");
  answer(f, 2, "snapshot", old.snapshot);
  await afterReads();
  answer(f, 3, "schedules", old.list, { tag: "old" });
  await afterReads();
  const newer = f.navigate(latestQuery);
  await completeList(f, 5, latest, "latest");
  await newer;
  answer(f, 4, "schedule_stats", old.stats);
  await older;
  expectLatest(f, "latest", { name: "latest-copy", work: "latest.echo" });
});

test("a late same-route snapshot cannot replace the published shared snapshot", async () => {
  const f = await mounted(), older = f.navigate(latestQuery), newer = f.navigate(latestQuery);
  await completeList(f, 3, latest, "latest");
  await newer;
  answer(f, 2, "snapshot", old.snapshot);
  await afterReads();
  const sharedBeforeTail = f.shared();
  if (f.calls.length > 6) {
    answer(f, 6, "schedules", old.list, { tag: "latest" });
    await afterReads();
    answer(f, 7, "schedule_stats", old.stats);
  }
  await older;
  assert.equal(sharedBeforeTail, "latest");
  expectLatest(f, "latest", { name: "latest-copy", work: "latest.echo" });
});

test("current list refusal retains its diagnostic after an older list rejects", async () => {
  const f = await mounted(), before = f.list(), older = f.navigate("#/schedules?tag=old");
  answer(f, 2, "snapshot", old.snapshot);
  await afterReads();
  const newer = f.navigate(latestQuery);
  answer(f, 4, "snapshot", latest.snapshot);
  await afterReads();
  f.calls[5].reject(new Error("current tagged list refused"));
  await newer;
  assert.equal(f.diagnostic(), "current tagged list refused");
  assert.equal(f.diagnosticRole(), "alert");
  f.calls[3].reject(new Error("obsolete tagged list refused"));
  await older;
  assert.equal(f.diagnostic(), "current tagged list refused");
  assert.deepEqual(f.globals, ["Failed / state may be stale: current tagged list refused"]);
  assert.deepEqual(f.list(), { ...before, form: null });
});

test("current Refresh statistics refusal remains visible with the last rendered list", async () => {
  const f = await mounted(), before = f.list(), refresh = f.refresh();
  await afterReads();
  answer(f, 2, "snapshot", latest.snapshot);
  await afterReads();
  answer(f, 3, "snapshot", latest.snapshot);
  await afterReads();
  assert.equal(f.calls[4].method, "schedule_stats");
  f.calls[4].reject(new Error("current history unavailable"));
  await refresh;
  assert.equal(f.diagnostic(), "current history unavailable");
  assert.equal(f.diagnosticRole(), "alert");
  assert.deepEqual(f.globals, ["Failed / state may be stale: current history unavailable"]);
  assert.deepEqual(f.list(), before);
});

test("a retired list result cannot replace the actual detail and its occurrence history", async () => {
  const f = await mounted(), older = f.navigate("#/schedules");
  answer(f, 2, "snapshot", old.snapshot);
  await afterReads();
  const detail = f.navigate("#/schedules/operator.team.latest");
  answer(f, 4, "schedule_show", latest.detail, { name: "operator.team.latest", limit: 20 });
  await detail;
  const before = f.detail();
  assert.match(before.head, /operator\.team\.latest/);
  assert.match(before.history, /retained result/);
  assert.deepEqual(before.links, ["#/runs/latest%2Ffinal"]);
  answer(f, 3, "schedule_stats", old.stats);
  await older;
  assert.equal(f.detailMounted(), true);
  assert.deepEqual(f.detail(), before);
});

test("opening schedule detail retires the create form but keeps the list on refusal", async () => {
  const f = await mounted(), before = f.list();
  assert.ok(before.form);
  const detail = f.navigate("#/schedules/operator.team.latest");
  const whileLoading = f.list();
  assert.equal(f.calls[2].method, "schedule_show");
  f.calls[2].reject(new Error("current detail refused"));
  await detail;
  assert.deepEqual(whileLoading, { ...before, form: null });
  assert.deepEqual(f.list(), { ...before, form: null });
  assert.equal(f.diagnostic(), "current detail refused");
});

test("a retired detail response cannot replace a newly mounted schedule form", async () => {
  const f = await mounted(), detail = f.navigate("#/schedules/operator.team.latest");
  const list = f.navigate(latestQuery);
  await completeList(f, 3, latest, "latest");
  await list;
  answer(f, 2, "schedule_show", latest.detail, { name: "operator.team.latest", limit: 20 });
  await detail;
  assert.equal(f.detailMounted(), false);
  expectLatest(f, "latest", { name: "latest-copy", work: "latest.echo" });
});

test("a retired list rejection cannot replace the latest detail refusal", async () => {
  const f = await mounted(), older = f.navigate("#/schedules?tag=old");
  answer(f, 2, "snapshot", old.snapshot);
  await afterReads();
  const detail = f.navigate("#/schedules/operator.team.latest");
  f.calls[4].reject(new Error("current detail refused"));
  await detail;
  f.calls[3].reject(new Error("retired list refused"));
  await older;
  assert.equal(f.diagnostic(), "current detail refused");
  assert.deepEqual(f.globals, ["Failed / state may be stale: current detail refused"]);
});

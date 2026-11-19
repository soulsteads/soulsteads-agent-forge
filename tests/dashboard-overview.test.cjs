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
const ui = fs.readFileSync(root + "/assets/forge-ui.js", "utf8");
const asset = fs.readFileSync(process.env.FORGE_OVERVIEW_VIEWS || root + "/assets/forge-views.js", "utf8");
const registration = asset.match(/^registerView\("overview",.*$/m);
const poll = ui.match(/setPoll: (\(fn, ms\) => \{[^}]+\})/);
assert(registration && poll);
const source = [
  section(ui, "function h(", "const when = "),
  section(ui, "const chip = ", "function newKey("),
  section(ui, "function alertBox(", "// ------------------------------------------------------------------ routing"),
  section(ui, "const ALERT_IDS = ", 'registerView("workflows"'),
  section(asset, "const table = ", "// ------------------------------------------------------------------ doctor"),
  section(asset, "// ------------------------------------------------------------------ doctor\n", "// ------------------------------------------------------------------ agents"),
  ui.match(/^const iso = .*$/m)[0],
  registration[0],
  "U.iso = iso;",
  `U.setPoll = ${poll[1]};`,
].join("\n");

class Element {
  constructor(tag, doc, textContent = "") {
    this.localName = tag;
    this.document = doc;
    this.textContent = textContent;
    this.attributes = {};
    this.children = [];
    this.listeners = {};
    this.parentElement = null;
  }
  setAttribute(key, value) {
    this.attributes[key] = String(value);
  }
  addEventListener(event, listener) { this.listeners[event] = listener; }
  append(...children) {
    this.children.push(...children);
    for (const child of children) if (child instanceof Element) child.parentElement = this;
  }
  replaceChildren(...children) {
    for (const child of this.children) if (child instanceof Element) child.parentElement = null;
    this.children = [];
    this.append(...children);
  }
  findAll(predicate) {
    return [
      ...(predicate(this) ? [this] : []),
      ...this.children.flatMap((child) => child instanceof Element ? child.findAll(predicate) : []),
    ];
  }

}
function offlinePage() {
  const doc = {
    activeElement: null,
    createElement: (tag) => new Element(tag, doc),
    createTextNode: (text) => new Element("#text", doc, text),
  };
  const body = doc.createElement("div"), diag = doc.createElement("div");
  const calls = [], statuses = [];
  const location = { hash: "#/overview" }, views = new Map();
  const timers = new Map();
  let timerId = 0;
  const by = (predicate) => body.findAll(predicate)[0];
  const $ = (id) => ({ "overview-body": body, "overview-alert": diag }[id] ??
    by((node) => node.attributes.id === id));
  const context = vm.createContext({
    document: doc, Node: Element, URLSearchParams, location, views, $,
    U: { parseHash: () => context.parseHash(), routeGeneration: () => vm.runInContext("routeGeneration", context) },
    fetch() { assert.fail("network access is unavailable"); },
    show() {}, cleanupPlugin() {}, pollTimer: null,
    setTimeout(fn, ms) { timers.set(++timerId, { fn, ms }); return timerId; },
    clearTimeout(id) { timers.delete(id); },
    registerView: (view, loader) => views.set(view, loader),
    F: {
      connected: () => true,
      status: (message) => statuses.push(message),
      call(method, params) {
        assert(["overview", "approvals"].includes(method), "only the two Overview reads are available");
        if (method === "overview") assert.equal(params, undefined);
        else assert.deepEqual(JSON.parse(JSON.stringify(params)), { state: "pending", limit: 100 });
        return new ContextPromise((resolve, reject) => calls.push({ method, resolve, reject }));
      },
    },
  });
  const ContextPromise = vm.runInContext("Promise", context);
  vm.runInContext(source, context);
  const visible = node => node.textContent + node.children.map(visible).join("");
  return {
    calls, statuses, $,
    navigate(hash) { location.hash = hash; return context.route(); },
    refresh: () => by((node) => node.localName === "button" && node.textContent === "Refresh").listeners.click(),
    diagnostic: () => diag.children[0]?.textContent || "",
    role: () => diag.children[0]?.attributes.role,
    snapshot() {
      const attention = by(node => node.attributes["aria-labelledby"] === "ov-attention-heading");
      const card = id => visible($(id));
      return {
        attention: visible(attention),
        health: card("ov-health"),
        failures: card("ov-failures"),
        upcoming: card("ov-upcoming"),
        links: body.findAll(node => node.localName === "a").map(node => node.attributes.href),
      };
    },
    timers: () => Array.from(timers, ([id, { ms }]) => [id, ms]),
    firePoll() {
      assert.equal(timers.size, 1);
      const [[id, { fn }]] = timers;
      timers.delete(id);
      fn();
    },
  };
}
const afterReads = () => new Promise((resolve) => setImmediate(resolve));


function sample(label, workCount, approvalCount) {
  return {
    overview: {
      health: { service: "ok", engine: "DBOS", database: label === "old" ? "failed" : "ok",
        scheduler: label + " scheduler", runners: [], plugins: [], time: label + " timestamp" },
      counts: { work: workCount, pending_approvals: approvalCount },
      failures: [
        { run_id: label + "/recovery", work_id: label + "-recover", status: "RECOVERY_REQUIRED",
          failure: { step: "approval", reason: label + " recovery detail" } },
        { run_id: label + "/failure", work_id: label + "-failed", status: "FAILED",
          failure: { step: "action", reason: label + " ordinary failure" } },
      ],
      upcoming: [{ schedule_name: label + "/scheduled", work_id: label + "-work",
        next_run: "2026-11-19T14:00:00Z", schedule: "0 0 14 * * *", cron_timezone: "UTC" }],
    },
    approvals: { approvals: Array.from({ length: approvalCount },
      () => ({ state: "pending", actionable: true })) },
  };
}
const initial = sample("initial", 2, 1);
const old = sample("old", 9, 4);
const latest = sample("latest", 3, 2);

function respond(f, offset, data) {
  assert.deepEqual(f.calls.slice(offset, offset + 2).map(({ method }) => method), ["overview", "approvals"]);
  f.calls[offset].resolve(data.overview);
  f.calls[offset + 1].resolve(data.approvals);
}
function expectLatest(f, expectedLinks = true) {
  const visible = f.snapshot();
  assert.match(visible.health, /latest schedulerWork3 definitionsApprovals2 pending/);
  assert.match(visible.attention, /2 actionable approvals shown\./);
  assert.match(visible.attention, /latest-recover — step approval: latest recovery detail/);
  assert.match(visible.failures, /step action: latest ordinary failure/);
  assert.match(visible.upcoming, /2026-11-19 14:00:00Z/);
  if (expectedLinks) assert.deepEqual(visible.links, [
    "#/approvals?state=pending", "#/runs/latest%2Frecovery", "#/approvals",
    "#/schedules/latest%2Fscheduled", "#/runs/latest%2Ffailure",
  ]);
}
async function mounted() {
  const f = offlinePage(), action = f.navigate("#/overview");
  respond(f, 0, initial);
  await action;
  return f;
}

test("newest ordinary Refresh publishes exact links, counts, diagnostics and Doctor control", async () => {
  const f = await mounted(), refresh = f.refresh();
  respond(f, 2, latest);
  await refresh;
  expectLatest(f);
  assert.equal(f.diagnostic(), "");
  assert.equal(f.$("ov-doctor-run").textContent, "Run doctor");
  assert.equal(typeof f.$("ov-doctor-run").onclick, "function");
  assert.deepEqual(f.timers().map(([, delay]) => delay), [8000]);
});

test("older poll cannot replace newer attention, health, failures or rearm", async () => {
  const f = await mounted();
  f.firePoll();
  f.calls[2].resolve(old.overview);
  const refresh = f.refresh();
  respond(f, 4, latest);
  await refresh;
  const timers = f.timers(), doctor = f.$("ov-doctor-run");
  f.calls[3].resolve(old.approvals);
  await afterReads();
  expectLatest(f);
  assert.deepEqual(f.timers(), timers);
  assert.equal(f.$("ov-doctor-run"), doctor);
});

test("older poll cannot publish or rearm after a genuine newer Refresh refusal", async () => {
  const f = await mounted(), previous = f.snapshot();
  f.firePoll();
  const refresh = f.refresh();
  f.calls[4].reject(new Error("current refresh refused"));
  f.calls[5].resolve(latest.approvals);
  await refresh;
  assert.equal(f.diagnostic(), "current refresh refused");
  assert.equal(f.role(), "alert");
  assert.deepEqual(f.statuses, []);
  respond(f, 2, old);
  await afterReads();
  assert.deepEqual(f.snapshot(), previous);
  assert.equal(f.diagnostic(), "current refresh refused");
  assert.deepEqual(f.timers(), []);
});

test("latest partial evidence keeps its pending approval fallback and imperfect failure data", async () => {
  const f = await mounted(), refresh = f.refresh();
  f.calls[2].resolve({ ...latest.overview, failures: [...latest.overview.failures, { status: "FAILED" }] });
  f.calls[3].reject(new Error("pending approvals unavailable"));
  await refresh;
  const visible = f.snapshot();
  assert.match(visible.attention, /Pending approvals are unavailable\./);
  assert.match(visible.attention, /Recent run information is incomplete\./);
  assert.match(visible.attention, /latest recovery detail/);
  assert.match(visible.health, /Work3 definitionsApprovals2 pending/);
  assert.match(visible.failures, /latest ordinary failure/);
  assert.equal(f.diagnostic(), "");
  assert.deepEqual(f.timers().map(([, delay]) => delay), [8000]);
});

test("older routed refusal cannot overwrite the remounted newer view or its global status", async () => {
  const f = await mounted(), earlier = f.navigate("#/overview");
  await f.navigate("#/agents");
  const remount = f.navigate("#/overview");
  respond(f, 4, latest);
  await remount;
  f.calls[2].reject(new Error("stale route refusal"));
  f.calls[3].resolve(old.approvals);
  await earlier;
  expectLatest(f);
  assert.equal(f.diagnostic(), "");
  assert.deepEqual(f.statuses, []);
});

test("completion after leaving cannot render or schedule an Overview poll", async () => {
  const f = await mounted(), previous = f.snapshot(), refresh = f.refresh();
  await f.navigate("#/agents");
  respond(f, 2, old);
  await refresh;
  assert.deepEqual(f.snapshot(), previous);
  assert.deepEqual(f.timers(), []);
});

test("a genuine routed Overview failure still reaches the pane and global status", async () => {
  const f = await mounted(), before = f.snapshot(), current = f.navigate("#/overview");
  f.calls[2].reject(new Error("current route refused"));
  f.calls[3].resolve(latest.approvals);
  await current;
  assert.equal(f.diagnostic(), "current route refused");
  assert.deepEqual(f.statuses, ["Failed / state may be stale: current route refused"]);
  assert.deepEqual(f.snapshot(), before);
  assert.deepEqual(f.timers(), []);
});

test("a current poll failure stays silent and does not rearm", async () => {
  const f = await mounted(), before = f.snapshot();
  f.firePoll();
  f.calls[2].reject(new Error("current poll refused"));
  f.calls[3].resolve(latest.approvals);
  await afterReads();
  assert.equal(f.diagnostic(), "");
  assert.deepEqual(f.statuses, []);
  assert.deepEqual(f.snapshot(), before);
  assert.deepEqual(f.timers(), []);
});

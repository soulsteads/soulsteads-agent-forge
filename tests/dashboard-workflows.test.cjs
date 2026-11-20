"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const asset = fs.readFileSync(process.env.FORGE_WORKFLOWS_UI || path.join(__dirname, "../assets/forge-ui.js"), "utf8");
function section(start, end) {
  const from = asset.indexOf(start), to = asset.indexOf(end, from + start.length);
  assert(from >= 0 && to > from);
  return asset.slice(from, to);
}
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.textContent = ""; this.listeners = {}; this.attrs = {}; this.value = ""; }
  setAttribute(name, value) { this.attrs[name] = value; if (name === "value") this.value = value; if (name === "disabled") this.disabled = true; }
  addEventListener(event, listener) { this.listeners[event] = listener; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  querySelector(selector) { return walk(this).find((el) => `#${el.attrs.id}` === selector) || null; }
  reportValidity() { return Boolean(this.value); }
}
const walk = (el) => [el, ...el.children.filter((c) => c instanceof Element).flatMap(walk)];

function page(query = "", read, openingSnapshot = { version: "opening" }) {
  const body = new Element("div"), diag = new Element("div");
  const calls = [], statuses = [];
  const views = new Map();
  const route = { view: "workflows", query: new URLSearchParams(query) };
  const context = vm.createContext({
    Node: Element, views, pollTimer: null, clearTimeout() {}, show() {}, cleanupPlugin() {},
    document: { createElement: (tag) => new Element(tag), createTextNode: (text) => Object.assign(new Element("text"), { textContent: text }) },
    $: (id) => id.endsWith("-body") ? body : id === "sc-create" ? body.querySelector("#sc-create") : diag,
    parseHash: () => route,
    when: String, newKey: () => "test-key", runTable: () => new Element("run-table"), iso: String,
    builder: () => Object.assign(new Element("builder"), { value: "new draft", attrs: { id: "wf-create" } }),
    go() {}, guarded: (_diag, fn) => fn(),
    F: { connected: () => true, status: (message) => statuses.push(message), call: read || ((method) => {
      assert.equal(method, "snapshot");
      return new Promise((resolve, reject) => calls.push({ resolve, reject }));
    }) },
    snapshot: openingSnapshot, STATUSES: [],
  });
  const start = asset.includes("let workflowListRead =") ? "let workflowListRead =" : "async function workflowList()";
  vm.runInContext(section("function h(", "const when =") + section("const chip =", "function newKey()") + section("function alertBox(", "function go(") + section(start, "const EXAMPLE =") + section("async function workflowDetail(", "// ------------------------------------------------------------------ runs") + section("async function runList(", "async function runDetail(") + section("let scheduleListRead =", "function scheduleEditCard(") + section("const ALERT_IDS =", 'registerView("workflows"'), context);
  return { body, diag, calls, statuses, route, dispatch: () => context.route(), load: () => context.workflowList(), snapshot: () => context.snapshot,
    detail: (id) => { route.id = id; return context.workflowDetail(id); },
    history: () => context.runList(route.query),
    schedules: () => { route.view = "schedules"; return context.scheduleList(route.query); },
    byId: (id) => walk(body).find((el) => el.attrs.id === id),
    error: () => diag.children[0]?.textContent || "" };
}
const reply = (call, version) => call.resolve({ version, work: [{ id: version, definition: { type: "workflow" } }], types: {} });

test("current workflow list renders and retains its snapshot", async () => {
  const f = page(), pending = f.load();
  reply(f.calls[0], "current"); await pending;
  assert.equal(f.snapshot().version, "current");
  assert.equal(f.byId("wf-table").tag, "table");
});

test("late older workflow list cannot replace the new draft or snapshot", async () => {
  const f = page("mode=new"), older = f.load(), newer = f.load();
  reply(f.calls[1], "newer"); await newer;
  const draft = f.body.children.at(-1); draft.value = "operator draft";
  reply(f.calls[0], "older"); await older;
  assert.equal(f.snapshot().version, "newer");
  assert.equal(f.body.children.at(-1), draft);
  assert.equal(draft.value, "operator draft");
});

test("library filters workflow IDs and enabled state without opening the builder", async () => {
  const f = page(), pending = f.load();
  f.calls[0].resolve({ types: {}, work: [
    { id: "daily-report", enabled: true, definition: { type: "workflow" } },
    { id: "weekly-report", enabled: false, definition: { type: "workflow" } },
    { id: "report-helper", enabled: true, definition: { type: "echo" } },
  ] });
  await pending;
  const ids = () => walk(f.byId("wf-table")).filter((el) => el.tag === "tr" && el.attrs["data-workflow"]).map((el) => el.attrs["data-workflow"]);
  assert.deepEqual(ids(), ["daily-report", "weekly-report"]);
  assert.equal(walk(f.body).some((el) => el.tag === "builder"), false);
  const search = f.byId("wf-search"), state = f.byId("wf-state");
  search.value = "REPORT"; search.listeners.input();
  state.value = "disabled"; state.listeners.change();
  assert.deepEqual(ids(), ["weekly-report"]);
  search.value = "NO MATCH"; search.listeners.input();
  assert.equal(f.byId("wf-empty").textContent, "No workflows match these filters.");
  assert.equal(f.calls.length, 1);
});

test("a pending library response cannot overwrite a separately opened builder", async () => {
  const f = page(), older = f.load();
  f.route.query.set("mode", "new");
  const author = f.load();
  reply(f.calls[1], "author"); await author;
  reply(f.calls[0], "library"); await older;
  assert.equal(f.body.children.at(-1).tag, "builder");
  assert.equal(f.snapshot().version, "author");
});

test("leaving the builder removes Create even when the library request fails", async () => {
  const f = page("mode=new"), opening = f.load();
  reply(f.calls[0], "author"); await opening;
  assert.ok(f.byId("wf-create"));
  f.route.query.delete("mode");
  const pending = f.load();
  const retainedWhileLoading = Boolean(f.byId("wf-create"));
  f.calls[1].reject(new Error("library unavailable")); await pending;
  assert.equal(retainedWhileLoading, false);
  assert.equal(f.byId("wf-create"), undefined);
  assert.equal(f.error(), "library unavailable");
});

test("workflow detail exposes full step inputs and only schedules for the exact workflow", async () => {
  const longInput = "a".repeat(200);
  const reads = {
    work: { id: "daily", enabled: true, definition: { type: "workflow", input: { steps: [
      { id: "prepare", action: { type: "echo", input: { text: longInput } } },
      { id: "finish", needs: ["prepare"], action: { work_id: "saved" } },
    ] } } },
    runs: { runs: [] },
    schedules: [
      { schedule_name: "daily-timer", context: { work_id: "daily" }, schedule: "0 5 * * *", status: "ACTIVE" },
      { schedule_name: "other-timer", context: { work_id: "daily-v2" } },
    ],
  };
  const f = page("", async (method) => reads[method]);
  await f.detail("daily");
  const nodes = walk(f.body);
  const steps = walk(f.byId("wf-structure")).filter((el) => el.className === "wf-step");
  assert.equal(steps.length, 2);
  assert.ok(walk(steps[0]).some((el) => el.tag === "pre" && el.textContent.includes(longInput)));
  assert.ok(walk(steps[1]).some((el) => el.textContent === "Reads from prepare"));
  assert.ok(nodes.some((el) => el.attrs.href === "#/schedules/daily-timer"));
  assert.ok(!nodes.some((el) => el.attrs.href === "#/schedules/other-timer"));
});

test("run history keeps an exact workflow filter outside the snapshot", async () => {
  const requests = [];
  const f = page("work=newly-registered", async (method, params) => {
    requests.push({ method, params: { ...params } });
    return { runs: [], scanned: 0 };
  }, { work: [] });
  await f.history();
  assert.ok(walk(f.body).some(el => el.tag === "option" && el.attrs.value === "newly-registered"));
  assert.deepEqual(requests, [{ method: "runs", params: { work_id: "newly-registered", limit: 25 } }]);
});

test("schedule creation resolves and submits the exact enabled workflow outside the snapshot", async () => {
  const requests = [], exact = { id: "older-workflow", enabled: true, definition: { type: "workflow" } };
  const f = page("work=older-workflow&name=weekday", async (method, params) => {
    requests.push({ method, params: { ...params } });
    if (method === "snapshot") return { schedules: [], work: [{ id: "other", enabled: true, definition: { type: "echo" } }] };
    if (method === "work") return exact;
    if (method === "schedule_stats") return { schedules: [] };
    if (method === "schedule") return [];
    assert.fail(`unexpected request ${method}`);
  });
  await f.schedules();
  const work = f.byId("sc-work");
  assert.ok(walk(work).some((el) => el.tag === "option" && el.attrs.value === exact.id));
  assert.equal(work.value, exact.id);
  await f.byId("sc-submit").listeners.click();
  assert.deepEqual(requests.filter((r) => r.method === "work"), [{ method: "work", params: { work_id: exact.id } }]);
  assert.equal(requests.find((r) => r.method === "schedule").params.work_id, exact.id);
});

test("a changed schedule route removes the old create form before a failed reload", async () => {
  const snapshots = [], writes = [];
  const f = page("work=previous&name=first", (method, params) => {
    if (method === "snapshot") return new Promise((resolve, reject) => snapshots.push({ resolve, reject }));
    if (method === "schedule_stats") return Promise.resolve({ schedules: [] });
    if (method === "schedule") { writes.push(params.work_id); return Promise.resolve([]); }
    assert.fail(`unexpected request ${method}`);
  });
  const opening = f.schedules();
  snapshots[0].resolve({ schedules: [], work: [{ id: "previous", enabled: true, definition: { type: "workflow" } }] });
  await opening;
  const oldButton = f.byId("sc-submit");
  assert.ok(oldButton);
  f.route.query.set("work", "current");
  const pending = f.schedules();
  const retainedWhileLoading = Boolean(f.byId("sc-submit"));
  await oldButton.listeners.click();
  snapshots[1].reject(new Error("schedule list unavailable")); await pending;
  assert.equal(retainedWhileLoading, false);
  assert.equal(f.byId("sc-submit"), undefined);
  assert.equal(f.error(), "schedule list unavailable");
  assert.deepEqual(writes, []);
});

test("a missing or disabled requested workflow requires an explicit schedule selection", async () => {
  for (const enabledWork of [null, { id: "unavailable", enabled: false, definition: { type: "workflow" } }]) {
    const writes = [];
    const f = page("work=unavailable&name=weekday", async (method, params) => {
      if (method === "snapshot") return { schedules: [], work: [{ id: "other", enabled: true, definition: { type: "echo" } }] };
      if (method === "work") {
        if (!enabledWork) throw new Error("Not found");
        return enabledWork;
      }
      if (method === "schedule_stats") return { schedules: [] };
      if (method === "schedule") { writes.push(params.work_id); return []; }
      assert.fail(`unexpected request ${method}`);
    });
    await f.schedules();
    const work = f.byId("sc-work");
    assert.equal(work.value, "");
    await f.byId("sc-submit").listeners.click();
    assert.deepEqual(writes, []);
    work.value = "other";
    await f.byId("sc-submit").listeners.click();
    assert.deepEqual(writes, ["other"]);
  }
});

test("leaving a list for detail preserves detail and canonical state", async () => {
  const f = page(), pending = f.load();
  f.route.id = "detail";
  const detail = new Element("detail"); f.body.replaceChildren(detail);
  reply(f.calls[0], "obsolete"); await pending;
  assert.equal(f.body.children[0], detail);
  assert.equal(f.snapshot().version, "opening");
});

test("leave and re-entry retire older list responses", async () => {
  const f = page(), older = f.load();
  f.route.view = "skills";
  f.route.view = "workflows";
  const current = f.load();
  reply(f.calls[1], "reentered"); await current;
  reply(f.calls[0], "obsolete"); await older;
  assert.equal(f.snapshot().version, "reentered");
});

test("older errors cannot overwrite a current actionable refusal", async () => {
  const f = page(), older = f.load(), current = f.load();
  f.calls[1].reject(new Error("current refusal")); await current;
  f.calls[0].reject(new Error("obsolete refusal")); await older;
  assert.equal(f.error(), "current refusal");
  assert.deepEqual(f.statuses, ["Failed / state may be stale: current refusal"]);
});

test("an error after leaving the list cannot pollute detail", async () => {
  const f = page(), pending = f.load();
  f.route.id = "detail";
  f.calls[0].reject(new Error("obsolete refusal")); await pending;
  assert.equal(f.error(), "");
  assert.equal(f.statuses.length, 0);
});

test("a departed list cannot publish before the return hash dispatches its loader", async () => {
  const f = page(), pending = f.load();
  f.route.view = "skills"; await f.dispatch();
  f.route.view = "workflows";
  reply(f.calls[0], "obsolete"); await pending;
  assert.equal(f.snapshot().version, "opening");
  assert.equal(f.body.children.length, 0);
});

for (const [destination, query] of [["library", ""], ["builder", "mode=new"]]) {
  test(`detail to ${destination}: retire controls while loading and clear the loading status on failure`, async () => {
    let rejectSnapshot;
    const f = page("", (method) => {
      if (method === "work") return { id: "daily", enabled: true, definition: { type: "workflow", input: { steps: [] } } };
      if (method === "runs") return { runs: [] };
      if (method === "schedules") return [];
      if (method === "snapshot") return new Promise((_, reject) => { rejectSnapshot = reject; });
      assert.fail(`unexpected request ${method}`);
    });
    const loading = () => walk(f.body).some((el) => el.attrs.role === "status" && el.textContent.startsWith("Loading"));
    await f.detail("daily");
    assert.ok(f.byId("wf-run") && f.byId("wf-toggle"));
    delete f.route.id;
    f.route.query = new URLSearchParams(query);
    const pending = f.load();
    assert.equal(f.byId("wf-run"), undefined);
    assert.equal(f.byId("wf-toggle"), undefined);
    assert.equal(loading(), true);
    rejectSnapshot(new Error("snapshot unavailable"));
    await pending;
    assert.equal(f.error(), "snapshot unavailable");
    assert.equal(loading(), false, "a failed request must not still say it is loading");
    assert.equal(f.byId("wf-run"), undefined);
    assert.equal(f.byId("wf-toggle"), undefined);
  });
}

test("a failed reload after a saved toggle reports in the mounted workflow alert", async () => {
  let workReads = 0;
  const f = page("", async (method) => {
    if (method === "work") {
      workReads += 1;
      if (workReads > 1) throw new Error("reload unavailable");
      return { id: "daily", enabled: true, definition: { type: "workflow", input: { steps: [] } } };
    }
    if (method === "enable") return {};
    if (method === "runs") return { runs: [] };
    if (method === "schedules") return [];
    assert.fail(`unexpected request ${method}`);
  });
  await f.detail("daily");
  await f.byId("wf-toggle").listeners.click();
  assert.match(f.error(), /reload unavailable/);
  assert.match(f.error(), /saved/);
  assert.deepEqual(f.statuses, ["Failed / state may be stale: reload unavailable"]);
});

test("a pending workflow toggle admits only one change, even if the same detail remounts", async () => {
  const writes = [], reloads = [];
  let saved = false;
  const record = (enabled) => ({ id: "daily", enabled, definition: { type: "workflow", input: { steps: [] } } });
  const f = page("", (method, params) => {
    if (method === "work") return saved ? new Promise((resolve) => reloads.push(resolve)) : record(true);
    if (method === "enable") return new Promise((resolve) => writes.push({ params, resolve }));
    if (method === "runs") return { runs: [] };
    if (method === "schedules") return [];
    assert.fail(`unexpected request ${method}`);
  });
  await f.detail("daily");
  const toggle = f.byId("wf-toggle");
  const action = toggle.listeners.click();
  assert.equal(writes.length, 1);
  assert.equal(toggle.disabled, true, "disable the button before awaiting the write");
  await toggle.listeners.click();
  await f.detail("daily");
  const remounted = f.byId("wf-toggle");
  assert.notEqual(remounted, toggle);
  assert.equal(remounted.disabled, true);
  await remounted.listeners.click();
  assert.equal(writes.length, 1);
  assert.equal(writes[0].params.enabled, false);
  saved = true;
  writes[0].resolve({});
  await new Promise(setImmediate);
  assert.equal(reloads.length, 1, "reconcile the mounted workflow after the save");
  reloads[0](record(false));
  await action;
  assert.equal(f.byId("wf-toggle").textContent, "Enable");
  assert.ok(!f.byId("wf-toggle").disabled);
});

test("an older work read cannot clear a newer detail for the same workflow", async () => {
  const reads = [];
  const f = page("", (method) => {
    if (method === "work") return new Promise((resolve, reject) => reads.push({ resolve, reject }));
    if (method === "runs") return { runs: [] };
    if (method === "schedules") return [];
    assert.fail(`unexpected request ${method}`);
  });
  const older = f.detail("daily"), newer = f.detail("daily");
  reads[1].resolve({ id: "daily", enabled: false, definition: { type: "workflow", input: { steps: [] } } });
  await newer;
  const toggle = f.byId("wf-toggle");
  assert.equal(toggle.textContent, "Enable");
  reads[0].reject(new Error("older read unavailable"));
  await older;
  assert.equal(f.byId("wf-toggle"), toggle);
  assert.equal(f.error(), "");
  assert.deepEqual(f.statuses, []);
});

test("non-workflow saved work still exposes its complete definition JSON", async () => {
  const f = page("", async (method) => {
    if (method === "work") return { id: "helper", enabled: true, definition: { type: "echo", input: { text: "hello" } } };
    if (method === "runs") return { runs: [] };
    if (method === "schedules") return [];
    assert.fail(`unexpected request ${method}`);
  });
  await f.detail("helper");
  const summaries = walk(f.body).filter((el) => el.tag === "summary").map((el) => el.textContent);
  assert.ok(summaries.includes("Complete definition JSON"));
  assert.ok(walk(f.body).some((el) => el.tag === "pre" && el.textContent.includes("hello")));
});

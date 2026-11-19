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
  constructor(tag) { this.tag = tag; this.children = []; this.textContent = ""; this.listeners = {}; }
  setAttribute() {}
  addEventListener(event, listener) { this.listeners[event] = listener; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
}
function page() {
  const body = new Element("div"), diag = new Element("div");
  const calls = [], statuses = [];
  const views = new Map();
  const route = { view: "workflows" };
  const context = vm.createContext({
    Node: Element, views, pollTimer: null, clearTimeout() {}, show() {}, cleanupPlugin() {},
    document: { createElement: (tag) => new Element(tag), createTextNode: (text) => Object.assign(new Element("text"), { textContent: text }) },
    $: (id) => id === "wf-body" ? body : diag,
    parseHash: () => route,
    when: String,
    builder: () => Object.assign(new Element("builder"), { value: "new draft" }),
    F: { connected: () => true, status: (message) => statuses.push(message), call: (method) => {
      assert.equal(method, "snapshot");
      return new Promise((resolve, reject) => calls.push({ resolve, reject }));
    } },
    snapshot: { version: "opening" },
  });
  const start = asset.includes("let workflowListRead =") ? "let workflowListRead =" : "async function workflowList()";
  vm.runInContext(section("function h(", "const when =") + section("function alertBox(", "// Shows the service") + section(start, "const EXAMPLE =") + section("const ALERT_IDS =", 'registerView("workflows"'), context);
  return { body, diag, calls, statuses, route, dispatch: () => context.route(), load: () => context.workflowList(), snapshot: () => context.snapshot,
    error: () => diag.children[0]?.textContent || "" };
}
const reply = (call, version) => call.resolve({ version, work: [{ id: version, definition: { type: "workflow" } }], types: {} });

test("current workflow list renders and retains its snapshot", async () => {
  const f = page(), pending = f.load();
  reply(f.calls[0], "current"); await pending;
  assert.equal(f.snapshot().version, "current");
  assert.equal(f.body.children.at(-1).tag, "builder");
});

test("late older workflow list cannot replace the new draft or snapshot", async () => {
  const f = page(), older = f.load(), newer = f.load();
  reply(f.calls[1], "newer"); await newer;
  const draft = f.body.children.at(-1); draft.value = "operator draft";
  reply(f.calls[0], "older"); await older;
  assert.equal(f.snapshot().version, "newer");
  assert.equal(f.body.children.at(-1), draft);
  assert.equal(draft.value, "operator draft");
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

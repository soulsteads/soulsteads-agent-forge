"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const ui = fs.readFileSync(process.env.FORGE_PLUGIN_UI || path.join(__dirname, "../assets/forge-ui.js"), "utf8");
function section(start, end) {
  const from = ui.indexOf(start), to = ui.indexOf(end, from + start.length);
  assert(from >= 0 && to > from);
  return ui.slice(from, to);
}
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.textContent = ""; }
  setAttribute() {}
  addEventListener() {}
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
}
const tick = () => new Promise(resolve => setImmediate(resolve));
function page() {
  const calls = [], imports = [], statuses = [], pluginFetches = [];
  const body = new Element("div"), diag = new Element("div");
  const route = { view: "x", id: "fixture", rest: "home" }, views = new Map();
  const context = vm.createContext({
    Node: Element, views, pollTimer: null, clearTimeout() {}, show() {}, renderPluginNav() {},
    document: { createElement: (tag) => new Element(tag), createTextNode: (text) => Object.assign(new Element("text"), { textContent: text }) },
    $: id => id === "x-body" ? body : diag,
    parseHash: () => route,
    snapshot: { marker: "opening", health: { installed_plugins: [] } },
    F: { connected: () => true, status: text => statuses.push(text),
      pluginFetch: (...args) => { pluginFetches.push(args); return Promise.resolve({}); },
      call: method => { assert.equal(method, "snapshot"); return new Promise((resolve, reject) => calls.push({ resolve, reject })); } },
    loadModule: () => new Promise((resolve, reject) => imports.push({ resolve, reject })),
  });
  const plugin = section("let activePlugin =", "const ALERT_IDS =");
  assert.equal((plugin.match(/await import\(/g) || []).length, 1);
  // Control the module-loader boundary; exercise the real route, mount and cleanup code.
  vm.runInContext(section("function h(", "const when =") + section("function alertBox(", "// Shows the service") + section("async function guarded(", "function go(") +
    section("const installedPlugins =", "function renderPluginNav()") + plugin.replace("await import(", "await loadModule(") + section("const ALERT_IDS =", 'registerView("workflows"'), context);
  views.set("x", r => context.pluginPage(r.id, r.rest));
  return { calls, imports, body, diag, statuses, pluginFetches, route,
    load: () => context.route(), snapshot: () => vm.runInContext("snapshot", context),
    leave: () => { route.view = "overview"; return context.route(); },
    error: () => diag.children[0]?.textContent || "" };
}
function snapshot(call, marker = "current") {
  call.resolve({ marker, health: { installed_plugins: [{ id: "fixture", version: "1", enabled: true,
    dashboard: [{ path: "home", label: "Fixture", entry: "/fixture.js" }] }] } });
}
async function imported() {
  const f = page(), pending = f.load(); snapshot(f.calls[0]); await tick();
  assert.equal(f.imports.length, 1);
  return { f, pending };
}

test("leaving a mounted plugin calls cleanup exactly once", async () => {
  const { f, pending } = await imported(); let cleaned = 0;
  f.imports[0].resolve({ mount: async () => () => ++cleaned }); await pending;
  await f.leave(); await f.leave();
  assert.equal(cleaned, 1);
});

test("a plugin snapshot after departure changes neither DOM nor shared state", async () => {
  const f = page(), pending = f.load(); await f.leave();
  snapshot(f.calls[0], "obsolete"); await pending;
  assert.equal(f.snapshot().marker, "opening");
  assert.equal(f.body.children.length, 0);
  assert.equal(f.imports.length, 0);
});

test("a module resolving after departure is never mounted", async () => {
  const { f, pending } = await imported(); let mounted = 0;
  await f.leave(); f.imports[0].resolve({ mount: () => ++mounted }); await pending;
  assert.equal(mounted, 0);
});

test("an async mount finishing after departure is immediately cleaned up", async () => {
  const { f, pending } = await imported(); let finish, host, cleaned = 0;
  f.imports[0].resolve({ mount: (el, api) => { host = api; return new Promise(resolve => { finish = resolve; }); } });
  await tick(); await f.leave();
  host.setStatus("obsolete status");
  await assert.rejects(host.fetch("status"), /no longer active/);
  finish(() => ++cleaned); await pending;
  assert.equal(cleaned, 1); assert.deepEqual(f.statuses, []); assert.equal(f.pluginFetches.length, 0);
});

test("an obsolete import error cannot replace the new page diagnostic", async () => {
  const { f, pending } = await imported(); await f.leave();
  f.imports[0].reject(new Error("obsolete import refusal")); await pending;
  assert.equal(f.error(), "");
});

test("a current import error remains actionable", async () => {
  const { f, pending } = await imported();
  f.imports[0].reject(new Error("current import refusal")); await pending;
  assert.equal(f.error(), "current import refusal");
});

test("reload cleans up the previous mount and owns only the new cleanup", async () => {
  const { f, pending } = await imported(); let first = 0, second = 0;
  f.imports[0].resolve({ mount: () => () => ++first }); await pending;
  const reload = f.load(); assert.equal(first, 1);
  snapshot(f.calls[1]); await tick();
  f.imports[1].resolve({ mount: () => () => ++second }); await reload;
  await f.leave(); assert.equal(first, 1); assert.equal(second, 1);
});

test("an async cleanup rejection on departure is contained", async () => {
  const { f, pending } = await imported();
  f.imports[0].resolve({ mount: () => async () => { throw Error("cleanup refusal"); } }); await pending;
  await f.leave(); await tick();
  assert.equal(f.error(), "");
});

test("an async cleanup rejection from a retired mount is contained", async () => {
  const { f, pending } = await imported(); let finish;
  f.imports[0].resolve({ mount: () => new Promise(resolve => { finish = resolve; }) });
  await tick(); await f.leave();
  finish(async () => { throw Error("retired cleanup refusal"); });
  await pending; await tick(); assert.equal(f.error(), "");
});

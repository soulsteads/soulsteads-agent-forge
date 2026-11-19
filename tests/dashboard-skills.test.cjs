"use strict";
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
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
const views = fs.readFileSync(process.env.FORGE_SKILLS_VIEWS || root + "/assets/forge-views.js", "utf8");
const registry = fs.readFileSync(root + "/src/skills.rs", "utf8");
const entries = section(registry, "const SKILLS:", "fn frontmatter(");
const matches = [...entries.matchAll(/"([^"]+)",\s*include_str!\("\.\.\/([^"]+)"\)/g)];
assert.deepEqual(matches.map(([, name, path]) => [name, path]),
  [["forge-operations", "skills/forge-operations/SKILL.md"]]);
const [[, name, path]] = matches;
const bytes = fs.readFileSync(root + "/" + path);
const content = bytes.toString("utf8");
const frontmatter = section(content, "---\n", "\n---");
const metadata = {
  name, bytes: bytes.length, sha256: crypto.createHash("sha256").update(bytes).digest("hex"),
  description: frontmatter.match(/^description: (.*)$/m)[1],
};
const display = { name, content, description: metadata.description, sha256: metadata.sha256 };

const registration = views.match(/^registerView\("skills",.*$/m);
assert(registration);
const source = [
  section(ui, "function h(", "const when = "),
  section(ui, "function pre(", "function newKey("),
  section(ui, "function alertBox(", "// ------------------------------------------------------------------ routing"),
  section(ui, "const ALERT_IDS = ", 'registerView("workflows"'),
  section(views, "const table = ", "const runLink = "),
  section(views, "// ------------------------------------------------------------------ skills\n", "// ------------------------------------------------------------------ providers"),
  registration[0],
].join("\n");

class Element {
  constructor(tag, textContent = "") {
    this.tag = tag;
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
      ...this.children.flatMap((child) => child instanceof Element ? child.findAll(predicate) : []),
    ];
  }
}

function offlinePage() {
  const body = new Element("div");
  const diag = new Element("div");
  const calls = [];
  const statuses = [];
  const location = { hash: "#/skills" };
  const by = (predicate) => body.findAll(predicate)[0];
  const viewMap = new Map();
  const context = vm.createContext({
    document: {
      createElement: (tag) => new Element(tag),
      createTextNode: (text) => new Element("#text", text),
    },
    Node: Element, URLSearchParams, location, views: viewMap,
    U: { parseHash: () => context.parseHash(), routeGeneration: () => vm.runInContext("routeGeneration", context), scrollTable: (label, table) => table },
    $: (id) => ({ "skills-body": body, "skills-alert": diag }[id] ??
      by((node) => node.attributes.id === id)),
    show() {}, cleanupPlugin() {}, clearTimeout() {}, pollTimer: null,
    registerView: (view, loader) => viewMap.set(view, loader),
    F: {
      connected: () => true,
      status: (message) => statuses.push(message),
      call(method, params) {
        if (method === "skills") {
          assert.equal(params, undefined);
        } else {
          assert.equal(method, "skill_show", "only inert Skills reads are available");
          assert.deepEqual(JSON.parse(JSON.stringify(params)), { name });
        }
        return new (vm.runInContext("Promise", context))((resolve, reject) => calls.push({ method, resolve, reject }));
      },
    },
  });
  vm.runInContext(source, context);
  return {
    calls, statuses,
    setHash(hash) { location.hash = hash; },
    navigate(hash) { location.hash = hash; return context.route(); },
    clickShow: () => by((node) => node.tag === "button" && node.textContent === "Show").listeners.click(),
    catalog: () => body.findAll((node) => node.attributes["data-skill"]).map((row) =>
      row.children.slice(0, 4).map((cell) => cell.children[0].textContent)),
    contentPane: () => by((node) => node.attributes.id === "sk-content"),
    diagnostic: () => diag.children[0]?.textContent || "",
    diagnosticRole: () => diag.children[0]?.attributes.role,
  };
}

function catalogReply(call) {
  assert.equal(call.method, "skills");
  call.resolve({ skills: [metadata] });
}

function showReply(call) {
  assert.equal(call.method, "skill_show");
  call.resolve(display);
}

async function mounted() {
  const f = offlinePage();
  const mount = f.navigate("#/skills");
  catalogReply(f.calls[0]);
  await mount;
  return f;
}

function assertSkill(f) {
  const [heading, text] = f.contentPane().children;
  assert.equal(heading.textContent, name);
  assert.equal(text.textContent, content);
}

test("the exact single-row catalog and current Show display all bundled text", async () => {
  const f = await mounted();
  assert.deepEqual(f.catalog(), [[name, metadata.description, `${metadata.bytes} bytes`,
    metadata.sha256]]);
  const action = f.clickShow();
  showReply(f.calls[1]);
  await action;
  assertSkill(f);
  assert.equal(f.diagnostic(), "");
});

test("a current refusal remains visible and a new successful Show clears it", async () => {
  const f = await mounted();
  const failed = f.clickShow();
  f.calls[1].reject(new Error("current read refused"));
  await failed;
  assert.equal(f.diagnostic(), "current read refused");
  assert.equal(f.diagnosticRole(), "alert");
  const current = f.clickShow();
  showReply(f.calls[2]);
  await current;
  assertSkill(f);
  assert.equal(f.diagnostic(), "");
});

test("outdated rejection cannot overwrite a newer successful Show on the same row", async () => {
  const f = await mounted();
  const older = f.clickShow();
  const newer = f.clickShow();
  showReply(f.calls[2]);
  await newer;
  assertSkill(f);
  f.calls[1].reject(new Error("stale first Show refusal"));
  await older;
  assertSkill(f);
  assert.equal(f.diagnostic(), "");
});

test("outdated success cannot publish content after a newer refusal", async () => {
  const f = await mounted();
  const older = f.clickShow();
  const newer = f.clickShow();
  f.calls[2].reject(new Error("latest Show refusal"));
  await newer;
  showReply(f.calls[1]);
  await older;
  assert.deepEqual(f.contentPane().children, []);
  assert.equal(f.diagnostic(), "latest Show refusal");
});

test("leaving Skills suppresses a pending rejection while its old pane still exists", async () => {
  const f = await mounted();
  const pending = f.clickShow();
  const pane = f.contentPane();
  await f.navigate("#/overview");
  f.calls[1].reject(new Error("stale refusal after leaving"));
  await pending;
  assert.equal(f.contentPane(), pane);
  assert.equal(f.diagnostic(), "");
  assert.deepEqual(f.statuses, []);
});

test("re-entering Skills invalidates an old Show before the catalog reply remounts the pane", async () => {
  const f = await mounted();
  const pending = f.clickShow();
  await f.navigate("#/overview");
  const reentered = f.navigate("#/skills");
  f.calls[1].reject(new Error("stale refusal during re-entry"));
  await pending;
  assert.equal(f.diagnostic(), "");
  catalogReply(f.calls[2]);
  await reentered;
  const current = f.clickShow();
  showReply(f.calls[3]);
  await current;
  assertSkill(f);
});

test("after remount, an old rejection cannot overlay the new pane diagnostic", async () => {
  const f = await mounted();
  const pending = f.clickShow();
  const oldPane = f.contentPane();
  await f.navigate("#/overview");
  const reentered = f.navigate("#/skills");
  catalogReply(f.calls[2]);
  await reentered;
  assert.notEqual(f.contentPane(), oldPane);
  const current = f.clickShow();
  f.calls[3].reject(new Error("new pane refusal"));
  await current;
  f.calls[1].reject(new Error("old pane refusal"));
  await pending;
  assert.equal(f.diagnostic(), "new pane refusal");
});


test("a Show refusal from the previous mounted pane stays out of the remounted pane", async () => {
  const f = await mounted();
  await f.navigate("#/overview");
  const reentered = f.navigate("#/skills");
  const oldPane = f.contentPane();
  const pending = f.clickShow();
  f.calls[2].reject(new Error("prior pane refusal"));
  catalogReply(f.calls[1]);
  await Promise.all([pending, reentered]);
  assert.notEqual(f.contentPane(), oldPane);
  assert.equal(f.diagnostic(), "");
});


test("a current catalog refusal survives a retired Show and the retained pane can recover", async () => {
  const f = await mounted();
  const oldPane = f.contentPane();
  await f.navigate("#/overview");
  const reentered = f.navigate("#/skills");
  const retired = f.clickShow();
  f.calls[1].reject(new Error("current catalog refusal"));
  await reentered;
  assert.equal(f.contentPane(), oldPane);
  assert.equal(f.diagnostic(), "current catalog refusal");
  f.calls[2].reject(new Error("retired Show refusal"));
  await retired;
  assert.equal(f.diagnostic(), "current catalog refusal");
  const current = f.clickShow();
  showReply(f.calls[3]);
  await current;
  assertSkill(f);
  assert.equal(f.diagnostic(), "");
});

test("a Show success from a pane awaiting replacement does not display stale content", async () => {
  const f = await mounted();
  const oldPane = f.contentPane();
  await f.navigate("#/overview");
  const reentered = f.navigate("#/skills");
  const retired = f.clickShow();
  showReply(f.calls[2]);
  await retired;
  assert.equal(oldPane.children.length, 0);
  catalogReply(f.calls[1]);
  await reentered;
  assert.notEqual(f.contentPane(), oldPane);
  assert.equal(f.contentPane().children.length, 0);
  assert.equal(f.diagnostic(), "");
});


test("current Show refusal survives late older catalog after second re-entry", async () => {
  const f = await mounted();
  await f.navigate("#/overview");
  const older = f.navigate("#/skills");
  await f.navigate("#/overview");
  const newer = f.navigate("#/skills");
  catalogReply(f.calls[2]);
  await newer;
  const show = f.clickShow();
  catalogReply(f.calls[1]);
  await older;
  f.calls[3].reject(new Error("current Show refusal"));
  await show;
  assert.equal(f.diagnostic(), "current Show refusal");
});

test("a prior Show cannot publish during the hash return before the re-entry loader", async () => {
  const f = await mounted();
  const pending = f.clickShow();
  await f.navigate("#/overview");
  f.setHash("#/skills");
  f.calls[1].reject(new Error("retired Show refusal"));
  await pending;
  assert.equal(f.diagnostic(), "");
});

test("a prior catalog cannot remount during the hash return before the re-entry loader", async () => {
  const f = await mounted();
  await f.navigate("#/overview");
  const pending = f.navigate("#/skills");
  await f.navigate("#/overview");
  f.setHash("#/skills");
  const pane = f.contentPane();
  catalogReply(f.calls[1]);
  await pending;
  assert.equal(f.contentPane(), pane);
});

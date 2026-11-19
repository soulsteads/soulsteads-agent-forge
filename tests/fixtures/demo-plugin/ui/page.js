// Test-only dashboard page. Trusted first-party model: it runs with the dashboard's authority
// and gets only the documented host API { pluginId, route, mount, fetch, setStatus }.
export async function mount(el, host) {
  const heading = document.createElement("p");
  heading.id = "demo-plugin-heading";
  heading.textContent = "Demo plugin page for " + host.pluginId;
  const out = document.createElement("pre");
  out.id = "demo-plugin-status";
  el.append(heading, out);
  const response = await host.fetch("status?from=page");
  const body = await response.json();
  out.textContent = "API status " + response.status + ": " + JSON.stringify(body);
  host.setStatus("Demo plugin page loaded");
  return () => { el.replaceChildren(); };
}

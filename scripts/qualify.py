"""Real DBOS/API/browser acceptance in operator-supplied isolated state.
Requires FORGE_DATABASE_URL (dedicated PostgreSQL), FORGE_TEST_STATE (fresh dir),
optional LOCAL_MODEL_BASE_URL/LOCAL_MODEL_ID, and an installed candidate.
"""

import argparse, json, os, subprocess, sys, time, uuid, hashlib, shutil
from pathlib import Path
import httpx


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--port", type=int, default=8110)
    p.add_argument("--browser", action="store_true")
    a = p.parse_args()
    state = Path(os.environ["FORGE_TEST_STATE"]).resolve()
    state.mkdir(parents=True, exist_ok=False)
    home = state / "home"
    evidence = []
    binary = Path(os.environ["FORGE_BINARY"]).resolve(strict=True)
    assert binary.read_bytes()[:4] == b"\x7fELF", "candidate must be a native ELF, not a Python launcher"
    pinned=state/"agent-forge"
    shutil.copy2(binary,pinned)
    binary=pinned
    cli = [str(binary), "--home", str(home)]
    metadata = {"binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "runtime": "native Rust DBOS", "source_revision": os.environ.get("FORGE_CANDIDATE_SHA", "uncommitted build"), "reference_only": "No Python Forge process or imported module participates; Python is only the test driver."}
    (state / "artifact.json").write_text(json.dumps(metadata, indent=2))
    subprocess.run(cli + ["init"], check=True, capture_output=True)
    cfg = json.loads((home / "config.json").read_text())
    token = cfg["actors"]["operator"]["token"]
    cfg["actors"]["observer"] = {
        "token": uuid.uuid4().hex,
        "capabilities": ["read"],
        "types": [],
    }
    if os.environ.get("LOCAL_MODEL_BASE_URL"):
        cfg["models"]["local"] = {
            "base_url": os.environ["LOCAL_MODEL_BASE_URL"],
            "model": os.environ["LOCAL_MODEL_ID"],
        }
    (home / "config.json").write_text(json.dumps(cfg))
    (home / "config.json").chmod(0o600)
    url = f"http://127.0.0.1:{a.port}"
    log = (state / "server.log").open("w")
    proc = None

    def start():
        nonlocal proc
        proc = subprocess.Popen(
            cli + ["serve", "--port", str(a.port)], stdout=log, stderr=log
        )
        for _ in range(120):
            if proc.poll() is not None:
                raise RuntimeError("server exited: " + str(proc.returncode))
            try:
                if httpx.get(url, timeout=1).status_code == 200:
                    return
            except httpx.TransportError:
                pass
            time.sleep(0.25)
        raise RuntimeError("startup timeout")

    def call(method, **params):
        r = httpx.post(
            url + "/api",
            headers={"Authorization": "Bearer " + token},
            json={"method": method, "params": params},
            timeout=120,
        )
        assert r.status_code == 200, (method, r.status_code, r.text)
        return r.json()["result"]

    def wait(rid, status="SUCCESS"):
        for _ in range(180):
            r = call("inspect", run_id=rid)
            if r["status"] not in ["PENDING", "ENQUEUED", "ADMITTED"]:
                assert r["status"] == status, r
                return r
            time.sleep(0.5)
        raise AssertionError("run timeout")

    def stop(crash=False):
        if proc and proc.poll() is None:
            proc.kill() if crash else proc.terminate()
            proc.wait(timeout=20)

    try:
        start()
        assert httpx.post(url + "/api", json={"method": "snapshot"}).status_code == 403
        rejected = httpx.post(url + "/api", headers={"Authorization":"Bearer "+token}, json={"method":"register", "params":{"work_id":"invalid", "definition":{"type":"not-installed", "input":{}}}})
        assert rejected.status_code in (400,403), rejected.text
        call(
            "register",
            work_id="echo",
            definition={"type": "echo", "input": {"text": "QUALIFIED_ECHO"}},
        )
        rid = call("run", work_id="echo", key="same")["run_id"]
        assert call("run", work_id="echo", key="same")["run_id"] == rid
        r = wait(rid)
        assert r["output"]["text"] == "QUALIFIED_ECHO"
        evidence.append({"case": "manual-duplicate", "run": r})
        call("enable", work_id="echo", enabled=False)
        disabled = httpx.post(url + "/api", headers={"Authorization":"Bearer "+token}, json={"method":"run", "params":{"work_id":"echo", "key":"disabled"}})
        assert disabled.status_code in (400,403), disabled.text
        call("enable", work_id="echo", enabled=True)
        changed = httpx.post(url + "/api", headers={"Authorization":"Bearer "+token}, json={"method":"register", "params":{"work_id":"echo", "definition":{"type":"echo", "input":{"text":"changed"}}}})
        assert changed.status_code in (400,409), changed.text
        evidence.append({"case":"immutable-definition-disable-enable","passed":True})
        other = httpx.post(
            url + "/api",
            headers={"Authorization": "Bearer " + cfg["actors"]["observer"]["token"]},
            json={"method": "inspect", "params": {"run_id": rid}},
        )
        assert other.status_code == 403
        call("register", work_id="failure", definition={"type": "fail", "input": {}})
        failed = call("run", work_id="failure", key="failure")["run_id"]
        evidence.append({"case": "diagnostic", "run": wait(failed, "ERROR")})
        retried = call("retry", run_id=failed, key="retry")["run_id"]
        assert wait(retried, "ERROR")["parent"] == failed
        call("schedule", name="due", work_id="echo", cron="*/2 * * * * *", tz="UTC")
        for _ in range(20):
            snapshot = call("snapshot")
            scheduled = [
                r
                for r in snapshot["runs"]
                if r["work_id"] == "echo" and r["run_id"] != rid
            ]
            if scheduled:
                break
            time.sleep(1)
        assert scheduled, "schedule did not fire"
        evidence.append(
            {"case": "scheduled", "runs": scheduled, "schedules": snapshot["schedules"]}
        )
        call("schedule_action", name="operator.due", action="pause")
        before={row["run_id"] for row in call("snapshot")["runs"]}
        call("schedule_action", name="operator.due", action="resume")
        for _ in range(40):
            resumed=[row for row in call("snapshot")["runs"] if row["run_id"] not in before]
            if resumed:break
            time.sleep(.2)
        assert resumed,"resumed schedule never fired"
        call("schedule_action", name="operator.due", action="pause")
        wrong_zone=httpx.post(url+"/api",headers={"Authorization":"Bearer "+token},json={"method":"schedule","params":{"name":"invalid-zone","work_id":"echo","cron":"*/2 * * * * *","tz":"Not/AZone"}})
        assert wrong_zone.status_code==400,wrong_zone.text
        call("schedule",name="iana-zone",work_id="echo",cron="*/2 * * * * *",tz="America/Chicago")
        assert "America/Chicago" in json.dumps(call("snapshot")["schedules"])
        call("schedule_action",name="operator.iana-zone",action="delete")
        evidence.append({"case":"schedule-resume-and-iana-validation","passed":True})
        call(
            "register",
            work_id="capacity",
            definition={"type": "echo", "input": {"text": "BOUNDED", "delay": 3}},
        )
        capacity_ids = [
            call("run", work_id="capacity", key=f"capacity-{i}")["run_id"]
            for i in range(6)
        ]
        observed = []
        for _ in range(30):
            states = [
                r["status"]
                for r in call("snapshot")["runs"]
                if r["run_id"] in capacity_ids
            ]
            observed.append(states)
            assert states.count("PENDING") <= 2, states
            if states.count("PENDING") == 2 and states.count("ENQUEUED") >= 4:
                break
            time.sleep(0.1)
        assert any(
            x.count("PENDING") == 2 and x.count("ENQUEUED") >= 4 for x in observed
        ), observed
        for item in capacity_ids:
            wait(item)
        evidence.append(
            {"case": "manual-capacity", "samples": observed, "run_ids": capacity_ids}
        )
        call(
            "register",
            work_id="delay",
            definition={"type": "echo", "input": {"text": "RECOVERED", "delay": 8}},
        )
        recovery = call("run", work_id="delay", key="recover")["run_id"]
        time.sleep(1)
        stop(crash=True)
        start()
        evidence.append({"case": "crash-recovery", "run": wait(recovery)})
        cancelled = call("run", work_id="delay", key="cancel")["run_id"]
        call("cancel", run_id=cancelled)
        evidence.append({"case": "cancel", "run": wait(cancelled, "CANCELLED")})
        if cfg["models"]:
            call(
                "register",
                work_id="model",
                definition={
                    "type": "model",
                    "input": {
                        "model": "local",
                        "prompt": "Reply exactly FORGE_LOCAL_OK",
                        "expect": "FORGE_LOCAL_OK",
                    },
                },
            )
            local = call("run", work_id="model", key="local")["run_id"]
            local_result=wait(local)
            assert local_result["output"]["text"].strip()=="FORGE_LOCAL_OK",local_result
            evidence.append({"case": "local-model", "run": local_result})
            call(
                "schedule",
                name="model-due",
                work_id="model",
                cron="*/5 * * * * *",
                tz="UTC",
            )
            scheduled_model = []
            for _ in range(60):
                scheduled_model = [
                    r
                    for r in call("snapshot")["runs"]
                    if r["work_id"] == "model" and r["run_id"] != local
                ]
                if scheduled_model:
                    break
                time.sleep(0.2)
            call("schedule_action", name="operator.model-due", action="pause")
            assert scheduled_model, "scheduled model did not fire"
            scheduled_result=wait(scheduled_model[0]["run_id"])
            assert scheduled_result["output"]["text"].strip()=="FORGE_LOCAL_OK",scheduled_result
            evidence.append({"case":"scheduled-local-model","run":scheduled_result})
        if a.browser:
            from playwright.sync_api import sync_playwright

            with sync_playwright() as pw:
                browser = pw.chromium.launch(
                    headless=True, executable_path=os.environ.get("BROWSER_EXECUTABLE")
                )
                page = browser.new_page()
                page.goto(url)
                page.locator("#token").fill(token)
                page.get_by_role("button", name="Connect", exact=True).click()
                page.wait_for_function(
                    "() => document.querySelector('#status').textContent.startsWith('Current as of')"
                )
                page.locator("#register [name=work_id]").fill("browser")
                page.locator("#register textarea").fill('{"text":"BROWSER_OK"}')
                page.get_by_role("button", name="Register work", exact=True).click()
                page.get_by_role("cell", name="browser", exact=True).wait_for()
                row = page.locator("#works tr").filter(
                    has=page.get_by_role("cell", name="browser", exact=True)
                )
                row.get_by_role("button", name="Run now", exact=True).click()
                page.wait_for_function(
                    "() => document.querySelector('#detail').textContent.includes('BROWSER_OK')"
                )
                page.locator("#schedule [name=name]").fill("browser-due")
                page.locator("#schedule [name=work_id]").fill("browser")
                page.locator("#schedule [name=cron]").fill("*/2 * * * * *")
                page.get_by_role("button", name="Create schedule", exact=True).click()
                scheduled_row = page.locator("#schedule-list p").filter(
                    has_text="operator.browser-due"
                )
                scheduled_row.wait_for()
                scheduled_row.get_by_role("button", name="pause", exact=True).click()
                page.wait_for_function(
                    "() => document.querySelector('#schedule-list').textContent.includes('PAUSED')"
                )
                scheduled_row.get_by_role("button", name="delete", exact=True).click()
                page.wait_for_function(
                    "() => !document.querySelector('#schedule-list').textContent.includes('operator.browser-due')"
                )
                page.locator("#run-list p").filter(has_text=failed).get_by_role(
                    "button", name="Inspect", exact=True
                ).click()
                page.wait_for_function(
                    "() => document.querySelector('#detail').textContent.includes('Controlled') || document.querySelector('#detail').textContent.includes('Execution failed')"
                )
                page.reload()
                page.locator("#token").fill(token)
                page.get_by_role("button", name="Connect", exact=True).click()
                page.get_by_role("cell", name="browser", exact=True).wait_for()
                page.screenshot(path=str(state / "dashboard.png"), full_page=True)
                page.set_viewport_size({"width": 390, "height": 844})
                page.screenshot(
                    path=str(state / "dashboard-mobile.png"), full_page=True
                )
                browser.close()
                evidence.append(
                    {
                        "case": "browser-register-run-schedules-failure-refresh",
                        "passed": True,
                    }
                )
        stop()
        start()
        assert call("inspect", run_id=rid)["output"]["text"] == "QUALIFIED_ECHO"
        evidence.append({"case": "restart-consistency", "run_id": rid})
        (state / "evidence.json").write_text(
            json.dumps(evidence, indent=2, default=str)
        )
        print(
            json.dumps(
                {"passed": len(evidence), "evidence": str(state / "evidence.json")}
            )
        )
    finally:
        stop()
        log.close()
        (state / "partial-evidence.json").write_text(
            json.dumps(evidence, indent=2, default=str)
        )


if __name__ == "__main__":
    main()

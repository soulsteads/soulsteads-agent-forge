//! Deterministic fault tests for harness workloads. The `stdio` fixtures below are
//! test-only fake harnesses; they never qualify Codex, Claude Code or OpenCode.
use crate::{
    core::{self, Core, hash},
    dbos::admission::{AdmissionStore, Envelope, Invocation},
    harness::RunnerProfile,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const COUNT: &str = "n=$(cat \"$COUNT\" 2>/dev/null || echo 0); echo $((n+1)) > \"$COUNT\"";

fn profile(home: &std::path::Path, body: &str, mode: &str, timeout: u64) -> RunnerProfile {
    let count = home.join("count");
    let script = format!(
        "COUNT={}; {COUNT}; cat > /dev/null; {body}",
        count.display()
    );
    serde_json::from_value(json!({"adapter":"stdio","executable":"/bin/sh","args":["-c",script],"permission_mode":mode,"timeout_seconds":timeout,"max_output_bytes":4096})).unwrap()
}
/// A direct child, a setsid descendant and a double-forked (reparented) setsid descendant.
fn detach_body(home: &std::path::Path) -> String {
    let pids = home.join("pids");
    format!(
        "rm -f {p}; sleep 120 & echo $! >> {p}; setsid sleep 120 & echo $! >> {p}; (setsid sleep 120 & echo $! >> {p}); wait",
        p = pids.display()
    )
}
fn read_pids(home: &std::path::Path) -> Vec<i32> {
    std::fs::read_to_string(home.join("pids"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}
fn alive(pid: impl TryInto<i32>) -> bool {
    let pid: i32 = pid.try_into().ok().unwrap();
    // A zombie awaiting reaping is dead for this purpose.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}
fn fixture() -> (tempfile::TempDir, Arc<Core>) {
    fixture_with("auto")
}
fn fixture_with(containment: &str) -> (tempfile::TempDir, Arc<Core>) {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let mut policy = core::load_policy(home.path()).unwrap();
    let h = home.path();
    let pid = h.join("pid");
    let profiles = [
        (
            "ok",
            profile(
                h,
                "echo hello > note.txt; echo '{\"answer\":\"42\"}'",
                "workspace-write",
                5,
            ),
        ),
        (
            "readonly",
            profile(h, "echo '{\"answer\":\"42\"}'", "read-only", 5),
        ),
        (
            "malformed",
            profile(h, "echo 'I did it, no JSON'", "read-only", 5),
        ),
        (
            "wrongshape",
            profile(h, "echo '{\"answer\":7}'", "read-only", 5),
        ),
        (
            "nonzero",
            profile(h, "echo boom >&2; exit 3", "read-only", 5),
        ),
        (
            "leaky",
            profile(
                h,
                "echo \"token=$LEAKED_TOKEN fence=$FORGE_TOKEN home=$HOME\" >&2; echo \"$(env | sort | tr '\\n' ' ')\" > env.txt; echo '{\"answer\":\"42\"}'; exit 1",
                "read-only",
                5,
            ),
        ),
        (
            "slow",
            profile(
                h,
                &format!("sleep 60 & echo $! > {}; wait", pid.display()),
                "read-only",
                1,
            ),
        ),
        ("detach", profile(h, &detach_body(h), "read-only", 20)),
        ("detach_short", profile(h, &detach_body(h), "read-only", 1)),
        (
            "many",
            profile(
                h,
                "i=0; while [ $i -lt 70 ]; do echo $i > f$i.txt; i=$((i+1)); done; head -c 5000000 /dev/zero > big.bin; echo '{\"answer\":\"42\"}'",
                "workspace-write",
                20,
            ),
        ),
        (
            "big",
            profile(h, "head -c 100000 /dev/zero | tr '\\0' x", "read-only", 5),
        ),
    ];
    for (name, mut p) in profiles {
        p.containment = containment.into();
        policy.runners.insert(name.into(), p);
    }
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(
            &crate::settings::Paths::defaults(home.path()),
            &policy.home_id,
        )
        .unwrap(),
    );
    let core = Core::new(
        crate::settings::Paths::defaults(home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap();
    (home, core)
}
fn admitted(core: &Core, key: &str) -> Envelope {
    let definition = json!({"type":"workflow","input":{"steps":[{"id":"one","action":{"type":"echo","input":{"text":"x"}}}]}});
    core.register("operator", "flow", definition.clone())
        .unwrap();
    let invocation=Invocation::from_json(&json!({"format_version":1,"home_id":core.policy.home_id,"scope":"operator","workflow_name":core::WORKFLOW,"request_id":key,"workflow_version":core::VERSION,"principal":"operator","workspace":"flow","plugin":"workflow","resource":"bounded-work-queue","policy_ref":"configured-actor","definition_sha256":hash(crate::canonical::to_vec(&definition).unwrap()),"action_sha256":core.action_hash("operator",&definition).unwrap(),"parameters":{},"evidence":[],"authorization_expires_at_ms":null}).to_string()).unwrap();
    core.store.admit(&invocation).unwrap().envelope
}
fn action(profile: &str) -> Value {
    json!({"type":"harness","input":{"profile":profile,"prompt":"Write the note.","inputs":{"seed":"s"},"output_schema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}}})
}
fn launches(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("count"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "0".into())
}

// Deterministic public-CLI emulator, never a real native harness/model process.
fn soulsteads_fixture(case: &str) -> (tempfile::TempDir, Arc<Core>) {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let executable = home.path().join("native-fixture");
    let script = r##"#!/usr/bin/python3
import sys, json, pathlib, os, tomllib, time
if sys.argv[1:] == ['--version']:
    print('soulsteads-harness protocol-fixture')
    sys.exit(0)
root = pathlib.Path(__file__).parent
count = root / 'count'
count.write_text(str(int(count.read_text()) + 1) if count.exists() else '1')
assert sys.argv[1] == '--config' and sys.argv[3] == '--home'
assert sys.argv[5:] == ['run', '--format', 'jsonl', '-']
config = tomllib.loads(pathlib.Path(sys.argv[2]).read_text())
assert config['agent']['id'] == os.environ['FORGE_INVOCATION_ID']
assert pathlib.Path(config['grants']['workspace']) == pathlib.Path.cwd()
assert config['grants']['tools'] == ['read_file', 'list_dir', 'write_file']
assert config.get('mcp_servers', []) == [] and config.get('skills', {}).get('roots', []) == []
assert 'Output contract' in sys.stdin.read()
case = (root / 'case').read_text()
if case == 'slow':
    (root / 'pid').write_text(str(os.getpid()))
    time.sleep(120)
events = [
 {'type':'session_started','agent_id':config['agent']['id'],'model':config['provider']['model'],'resumed':False,'repaired_tail':False},
 {'type':'run_started','system_sha256':'a'*64},
 {'type':'model_request','turn':1,'messages':2,'tools':3},
 {'type':'model_response','turn':1,'content_bytes':0,'tool_calls':1,'finish_reason':'tool_calls','served_model':None,'prompt_tokens':None,'completion_tokens':None},
 {'type':'assistant_message','content':None},
 {'type':'tool_proposed','attempt_id':'a1','call_id':'c1','name':'write_file','arguments':{'path':'note.txt','content':'hello\n'},'effect':'mutating','approval':'allow'},
 {'type':'tool_attempt','attempt_id':'a1','call_id':'c1','name':'write_file','arguments':{'path':'note.txt','content':'hello\n'}},
 {'type':'tool_result','attempt_id':'a1','call_id':'c1','name':'write_file','status':'ok','output':'written','truncated':False},
 {'type':'model_request','turn':2,'messages':4,'tools':3},
 {'type':'model_response','turn':2,'content_bytes':15,'tool_calls':0,'finish_reason':'stop','served_model':None,'prompt_tokens':None,'completion_tokens':None},
 {'type':'assistant_message','content':'{"answer":"42"}'},
 {'type':'run_finished','status':'completed','reason':None,'final_text':'{"answer":"42"}','turns':2,'tool_calls':1},
]
if case.startswith('deadline'):
    events = events[:3] + [{'type':'run_finished','status':'timed_out','reason':'deadline_exceeded','final_text':None,'turns':1,'tool_calls':0}]
else:
    pathlib.Path('note.txt').write_text('hello\n')
if case in ['cancelled','uncertain','failed']:
    events[-1]['status'] = case
if case == 'wrongshape':
    events[-1]['final_text'] = events[-2]['content'] = '{"answer":7}'
for i, event in enumerate(events):
    event.update(seq=i+1, ts_ms=1234, session_id='native-session', run_id=None if i == 0 else 'native-run')
    if case in ['foreign', 'deadline-foreign'] and i == len(events)-1: event['session_id'] = 'other-session'
    print(json.dumps(event))
if case == 'malformed': print('not json')
if case == 'error-tail': print('{"type":"error","code":"late_failure"}')
if case == 'nonzero': sys.exit(3)
if case == 'deadline-malformed': print('not json')
if case.startswith('deadline'): sys.exit(5)
"##;
    std::fs::write(&executable, script).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(home.path().join("case"), case).unwrap();
    let mut policy = core::load_policy(home.path()).unwrap();
    policy.runners.insert("native".into(), serde_json::from_value(json!({
        "adapter":"soulsteads", "executable":executable, "model":"fixture-model",
        "permission_mode":"workspace-write", "timeout_seconds":if case == "slow" { 1 } else { 5 },
        "env":{"HARNESS_BASE_URL":"http://127.0.0.1:1/v1"}, "env_passthrough":[], "containment":"scan"
    })).unwrap());
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(
            &crate::settings::Paths::defaults(home.path()),
            &policy.home_id,
        )
        .unwrap(),
    );
    let core = Core::new(
        crate::settings::Paths::defaults(home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap();
    (home, core)
}

#[tokio::test]
async fn soulsteads_deadline_diagnostic_preserves_rejected_uncertain_effect() {
    for case in [
        "deadline",
        "deadline-malformed",
        "deadline-foreign",
        "nonzero",
    ] {
        let (home, core) = soulsteads_fixture(case);
        let e = admitted(&core, case);
        let mut a = action("native");
        a["input"]["permissions"] = json!("workspace-write");
        assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
        let effects = core.flow_effect_observations(&e).unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0]["state"], "uncertain");
        assert_eq!(effects[0]["output"], Value::Null);
        assert_eq!(effects[0]["reconciliation"], Value::Null);
        let attempt = &effects[0]["harness_attempt"];
        assert_eq!(attempt["state"], "exited");
        assert_eq!(attempt["result"], "rejected");
        assert_eq!(attempt["exit_code"], if case == "nonzero" { 3 } else { 5 });
        assert_eq!(
            attempt["failure"],
            if case == "deadline" {
                "harness exited nonzero; native timed_out/deadline_exceeded"
            } else {
                "harness exited nonzero"
            }
        );
        let dir = core.harness_dir(attempt["invocation_id"].as_str().unwrap());
        assert!(!dir.join("captured.json").exists());
        if case.starts_with("deadline") {
            assert!(!dir.join("workspace/note.txt").exists());
        }
        assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
        assert_eq!(core.flow_effect_observations(&e).unwrap(), effects);
        assert_eq!(launches(home.path()), "1");
    }
}

#[tokio::test]
async fn soulsteads_receipt_artifact_and_replay_use_existing_effect_boundary() {
    let (home, core) = soulsteads_fixture("ok");
    let e = admitted(&core, "native-ok");
    let mut a = action("native");
    a["input"]["permissions"] = json!("workspace-write");
    let result = core.flow_effect(&e, "root/step:h", &a).await.unwrap();
    assert_eq!(result["result"]["answer"], "42");
    assert_eq!(result["harness"]["adapter"], "soulsteads");
    assert_eq!(result["harness"]["session_id"], "native-session");
    assert_eq!(result["harness"]["native_run_id"], "native-run");
    assert_eq!(result["harness"]["tool_events"], 1);
    assert_eq!(result["artifacts"][0]["sha256"], hash("hello\n"));
    assert_eq!(result["artifacts"][0]["text"], "hello\n");
    let invocation = result["harness"]["invocation_id"].as_str().unwrap();
    assert_eq!(
        result["harness"]["config_sha256"],
        hash(std::fs::read(core.harness_dir(invocation).join("native.toml")).unwrap())
    );
    core.flow_verify_result(&e, "root/step:h", &a, &result)
        .unwrap();
    assert_eq!(
        core.flow_effect(&e, "root/step:h", &a).await.unwrap(),
        result
    );
    let receipts = core.flow_effect_observations(&e).unwrap();
    core.fence_flow(&e).unwrap();
    assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
    assert_eq!(core.flow_effect_observations(&e).unwrap(), receipts);
    assert_eq!(launches(home.path()), "1");
}

#[tokio::test]
async fn soulsteads_refusals_happen_before_workspace_or_effect_creation() {
    let (home, mut core) = soulsteads_fixture("ok");
    Arc::get_mut(&mut core)
        .unwrap()
        .policy
        .runners
        .get_mut("native")
        .unwrap()
        .permission_mode = "read-only".into();
    let e = admitted(&core, "permission-ceiling");
    let mut a = action("native");
    a["input"]["permissions"] = json!("workspace-write");
    assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
    assert!(core.flow_effect_observations(&e).unwrap().is_empty());
    for required in ["mcp_tools", "served_model_observation", "usage_reporting"] {
        let e = admitted(&core, required);
        let mut a = action("native");
        a["input"]["require"] = json!([required]);
        assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
        assert!(core.flow_effect_observations(&e).unwrap().is_empty());
    }
    for field in [
        "command",
        "executable",
        "args",
        "env",
        "grants",
        "config",
        "session",
    ] {
        let mut a = action("native");
        a["input"][field] = json!("caller-choice");
        assert!(core.register("operator", field, a).is_err(), "{field}");
    }
    assert_eq!(launches(home.path()), "0");
    assert!(!home.path().join("state/harness").exists());
}

#[tokio::test]
async fn soulsteads_cancellation_retains_unknown_effect_and_kills_fixture() {
    let (home, core) = soulsteads_fixture("slow");
    let e = admitted(&core, "native-cancel");
    let mut a = action("native");
    a["input"]["permissions"] = json!("workspace-write");
    let task = {
        let core = core.clone();
        let e = e.clone();
        let a = a.clone();
        tokio::spawn(async move { core.flow_effect(&e, "root/step:h", &a).await })
    };
    for _ in 0..100 {
        if home.path().join("pid").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let pid: u32 = std::fs::read_to_string(home.path().join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    core.fence_flow(&e).unwrap();
    let failure = task.await.unwrap().unwrap_err();
    assert!(!alive(pid));
    let effects = core.flow_effect_observations(&e).unwrap();
    assert_eq!(effects[0]["state"], "uncertain");
    // Existing scan containment may reap the direct child before Tokio waits.
    // Preserve that failure, then prove startup reconciliation never adopts it.
    if effects[0]["harness_attempt"]["state"] == "running" {
        assert_eq!(
            failure
                .downcast_ref::<std::io::Error>()
                .and_then(|e| e.raw_os_error()),
            Some(libc::ECHILD),
            "{failure:#}"
        );
        core.reconcile_harness_attempts().unwrap();
        let recovered = core.flow_effect_observations(&e).unwrap();
        assert_eq!(recovered[0]["harness_attempt"]["state"], "process_lost");
        assert_eq!(recovered[0]["state"], "uncertain");
    } else {
        assert_eq!(
            effects[0]["harness_attempt"]["state"], "cancelled",
            "{failure:#}"
        );
    }
    assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
    assert_eq!(launches(home.path()), "1");
}

#[tokio::test]
async fn soulsteads_faults_preserve_uncertainty_and_forbid_replacement() {
    for case in [
        "malformed",
        "wrongshape",
        "foreign",
        "cancelled",
        "uncertain",
        "failed",
        "error-tail",
        "nonzero",
        "slow",
    ] {
        let (home, core) = soulsteads_fixture(case);
        let e = admitted(&core, case);
        let mut a = action("native");
        a["input"]["permissions"] = json!("workspace-write");
        assert!(
            core.flow_effect(&e, "root/step:h", &a).await.is_err(),
            "{case}"
        );
        assert!(
            core.flow_effect(&e, "root/step:h", &a).await.is_err(),
            "{case}"
        );
        assert_eq!(launches(home.path()), "1", "{case}");
        let effects = core.flow_effect_observations(&e).unwrap();
        assert_eq!(effects[0]["state"], "uncertain", "{case}");
        if case == "slow" {
            let pid: u32 = std::fs::read_to_string(home.path().join("pid"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(!alive(pid));
        }
    }
}

#[tokio::test]
async fn completed_result_is_validated_receipted_and_reused_without_relaunch() {
    let (home, core) = fixture();
    let e = admitted(&core, "ok");
    let mut a = action("ok");
    a["input"]["permissions"] = json!("workspace-write");
    let first = core.flow_effect(&e, "root/step:h", &a).await.unwrap();
    assert_eq!(first["result"]["answer"], "42");
    assert_eq!(first["harness"]["adapter"], "stdio");
    assert_eq!(first["harness"]["attempt"], 1);
    assert_eq!(first["artifacts"][0]["path"], "note.txt");
    assert_eq!(first["artifacts"][0]["text"], "hello\n");
    assert_eq!(launches(home.path()), "1");
    let again = core.flow_effect(&e, "root/step:h", &a).await.unwrap();
    assert_eq!(again, first);
    assert_eq!(launches(home.path()), "1", "replay must reuse the receipt");
    core.flow_verify_result(&e, "root/step:h", &a, &first)
        .unwrap();
}

#[tokio::test]
async fn failures_leave_effect_uncertain_and_never_relaunch() {
    for (name, expected) in [("malformed", "1"), ("wrongshape", "1"), ("nonzero", "1")] {
        let (home, core) = fixture();
        let e = admitted(&core, name);
        let a = action(name);
        assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
        assert!(core.flow_effect(&e, "root/step:h", &a).await.is_err());
        assert_eq!(
            launches(home.path()),
            expected,
            "{name} must not be relaunched"
        );
        let effects = core.flow_effect_observations(&e).unwrap();
        assert_eq!(effects[0]["state"], "uncertain");
        assert_eq!(effects[0]["harness_attempt"]["state"], "exited");
    }
}

#[tokio::test]
async fn timeout_terminates_the_process_tree_and_requires_reconciliation() {
    let (home, core) = fixture();
    let e = admitted(&core, "slow");
    let started = std::time::Instant::now();
    assert!(
        core.flow_effect(&e, "root/step:h", &action("slow"))
            .await
            .is_err()
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let pid: i32 = std::fs::read_to_string(home.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!alive(pid), "descendant must be terminated");
    let effects = core.flow_effect_observations(&e).unwrap();
    assert_eq!(effects[0]["state"], "uncertain");
    assert_eq!(effects[0]["harness_attempt"]["state"], "timed_out");
    assert!(
        core.flow_effect(&e, "root/step:h", &action("slow"))
            .await
            .is_err()
    );
    assert_eq!(launches(home.path()), "1");
}

#[tokio::test]
async fn cancellation_fence_terminates_the_tree_including_setsid_descendants() {
    for mode in ["cgroup", "scan"] {
        let (home, core) = fixture_with(mode);
        let e = admitted(&core, "cancel");
        let a = action("detach");
        let runner = {
            let core = core.clone();
            let e = e.clone();
            let a = a.clone();
            tokio::spawn(async move { core.flow_effect(&e, "root/step:h", &a).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let pids = read_pids(home.path());
        assert_eq!(
            pids.len(),
            3,
            "{mode}: setsid and double-fork descendants must exist"
        );
        assert!(
            pids.iter().all(|p| alive(*p)),
            "{mode}: descendants running before cancel"
        );
        core.fence_flow(&e).unwrap();
        assert!(runner.await.unwrap().is_err());
        let effects = core.flow_effect_observations(&e).unwrap();
        assert_eq!(
            effects[0]["harness_attempt"]["state"], "cancelled",
            "{mode}"
        );
        assert_eq!(effects[0]["state"], "uncertain");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            pids.iter().all(|p| !alive(*p)),
            "{mode}: every descendant must be dead"
        );
        assert_eq!(launches(home.path()), "1");
    }
}

#[tokio::test]
async fn timeout_kills_setsid_descendants() {
    for mode in ["cgroup", "scan"] {
        let (home, core) = fixture_with(mode);
        let e = admitted(&core, "detach-timeout");
        assert!(
            core.flow_effect(&e, "root/step:h", &action("detach_short"))
                .await
                .is_err()
        );
        let pids = read_pids(home.path());
        assert_eq!(pids.len(), 3);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            pids.iter().all(|p| !alive(*p)),
            "{mode}: descendants must die on timeout"
        );
        assert_eq!(
            core.flow_effect_observations(&e).unwrap()[0]["harness_attempt"]["state"],
            "timed_out"
        );
    }
}

#[test]
fn orphaned_attempt_is_terminated_on_restart_and_pid_reuse_is_not_touched() {
    use crate::containment::{self, Contained, Mode};
    for mode in [Mode::Cgroup, Mode::Scan] {
        let (home, core) = fixture();
        // Simulate a Forge crash: a harness tree launched by a previous process.
        let invocation = Core::harness_invocation("run-orphan", "root/step:h", 1);
        let dir = core.harness_dir(&invocation);
        crate::safety::private_mkdir(&home.path().join("state/harness")).unwrap();
        crate::safety::private_mkdir(&dir).unwrap();
        let mut contained = Contained::create(mode, &invocation).unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "setsid sleep 120 & sleep 120"])
            .env("FORGE_INVOCATION_ID", &invocation);
        contained.prepare(&mut command);
        let child = command.spawn().unwrap();
        let pid = child.id();
        contained.set_root(pid);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let ticks = containment::start_ticks(pid).unwrap();
        let attempt = json!({"invocation_id":invocation,"run_id":"run-orphan","state":"running","pid":pid,"pid_start_ticks":ticks,"boot_id":containment::boot_id(),"cgroup":contained.cgroup});
        std::fs::write(dir.join("attempt.json"), attempt.to_string()).unwrap();
        contained.disarm(); // the "crashed" process never kills anything
        assert!(
            containment::alive(&invocation, contained.cgroup.as_deref(), Some((pid, ticks)))
                .unwrap()
        );
        let report = core.reconcile_harness_attempts().unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0]["state"], "orphan_terminated", "{mode:?}");
        assert!(
            !containment::alive(&invocation, contained.cgroup.as_deref(), Some((pid, ticks)))
                .unwrap(),
            "{mode:?}"
        );
        let stored: Value =
            serde_json::from_slice(&std::fs::read(dir.join("attempt.json")).unwrap()).unwrap();
        assert_eq!(stored["state"], "orphan_terminated");
        // Idempotent: a settled attempt is never rewritten or acted on again.
        assert!(core.reconcile_harness_attempts().unwrap().is_empty());
        let mut child = child;
        let _ = child.wait();
    }
    // Stale record: the pid is alive but is a different process (reuse). It must survive.
    let (home, core) = fixture();
    let invocation = Core::harness_invocation("run-reuse", "root/step:h", 1);
    let dir = core.harness_dir(&invocation);
    crate::safety::private_mkdir(&home.path().join("state/harness")).unwrap();
    crate::safety::private_mkdir(&dir).unwrap();
    let mut bystander = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let attempt = json!({"invocation_id":invocation,"run_id":"run-reuse","state":"running","pid":bystander.id(),"pid_start_ticks":1,"boot_id":crate::containment::boot_id(),"cgroup":null});
    std::fs::write(dir.join("attempt.json"), attempt.to_string()).unwrap();
    let report = core.reconcile_harness_attempts().unwrap();
    assert_eq!(report[0]["state"], "process_lost");
    assert!(
        alive(bystander.id()),
        "unrelated process with a recycled pid must not be killed"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
}

#[test]
fn claude_permissions_are_derived_from_the_profile_and_cannot_be_overridden() {
    let mut p: RunnerProfile = serde_json::from_value(
        json!({"adapter":"claude","executable":"/bin/true","permission_mode":"read-only"}),
    )
    .unwrap();
    p.validate("c").unwrap();
    for flag in [
        "--permission-mode=acceptEdits",
        "--tools",
        "--allowedTools",
        "--restricted",
        "--permission-prompts",
        "--mcp-config",
        "--dangerously-skip-permissions",
        "--add-dir",
    ] {
        p.args = vec![flag.into()];
        assert!(p.validate("c").is_err(), "{flag}");
    }
    let listing = p.describe("c");
    assert!(
        listing["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c != "mcp_tools")
    );
}

#[test]
fn runner_listing_exposes_no_paths_arguments_or_values() {
    let p: RunnerProfile = serde_json::from_value(json!({"adapter":"codex","executable":"/opt/private/bin/codex","args":["--config","k=secretish"],"env":{"CODEX_HOME":"/private/home"}})).unwrap();
    p.validate("c").unwrap();
    let text = p.describe("c").to_string();
    assert!(
        !text.contains("/opt/private")
            && !text.contains("secretish")
            && !text.contains("/private/home")
    );
    assert!(text.contains("codex") && text.contains("CODEX_HOME"));
}

#[tokio::test]
async fn oversized_or_unreadable_workspace_is_a_manifest_fact_not_a_failure() {
    let (_home, core) = fixture();
    let e = admitted(&core, "many");
    let mut a = action("many");
    a["input"]["permissions"] = json!("workspace-write");
    let out = core.flow_effect(&e, "root/step:h", &a).await.unwrap();
    assert_eq!(out["result"]["answer"], "42");
    let manifest = out["artifacts"].as_array().unwrap();
    assert!(manifest.iter().any(|m| m["truncated"] == true));
    assert!(manifest.iter().any(|m| m["oversize"] == true));
    assert_eq!(
        core.flow_effect_observations(&e).unwrap()[0]["state"],
        "completed"
    );
}

#[tokio::test]
async fn output_size_is_bounded_and_environment_secrets_are_not_inherited_or_retained() {
    let (home, core) = fixture();
    let e = admitted(&core, "big");
    assert!(
        core.flow_effect(&e, "root/step:h", &action("big"))
            .await
            .is_err()
    );
    assert_eq!(
        core.flow_effect_observations(&e).unwrap()[0]["harness_attempt"]["state"],
        "output_exceeded"
    );
    // SAFETY: tests in this module do not read these variables concurrently.
    unsafe {
        std::env::set_var("LEAKED_TOKEN", "super-secret-value-1234");
        std::env::set_var("FORGE_TOKEN", "forge-token-value-should-not-pass");
    }
    let e = admitted(&core, "leaky");
    assert!(
        core.flow_effect(&e, "root/step:h", &action("leaky"))
            .await
            .is_err()
    );
    let attempt = &core.flow_effect_observations(&e).unwrap()[0]["harness_attempt"];
    let text = attempt.to_string();
    assert!(!text.contains("super-secret") && !text.contains("forge-token-value"));
    let dir = core.harness_dir(attempt["invocation_id"].as_str().unwrap());
    let env = std::fs::read_to_string(dir.join("workspace/env.txt")).unwrap();
    assert!(
        !env.contains("LEAKED_TOKEN")
            && !env.contains("FORGE_TOKEN=")
            && env.contains("FORGE_RUN_ID")
    );
    drop(home);
}

#[test]
fn unknown_profile_executable_injection_permission_and_capability_refuse_before_dispatch() {
    let (home, core) = fixture();
    let register =
        |input: Value| core.register("operator", "h", json!({"type":"harness","input":input}));
    let schema = json!({"type":"object"});
    assert!(register(json!({"profile":"missing","prompt":"x","output_schema":schema})).is_err());
    assert!(register(json!({"profile":"/bin/sh","prompt":"x","output_schema":schema})).is_err());
    assert!(
        register(json!({"profile":"ok","prompt":"x","output_schema":schema,"command":["/bin/sh"]}))
            .is_err()
    );
    assert!(
        register(json!({"profile":"ok","prompt":"x","output_schema":schema,"env":{"A":"b"}}))
            .is_err()
    );
    assert!(register(json!({"profile":"readonly","prompt":"x","output_schema":schema,"permissions":"workspace-write"})).is_err());
    assert!(
        register(
            json!({"profile":"ok","prompt":"x","output_schema":schema,"require":["tool_use"]})
        )
        .is_err()
    );
    assert!(register(json!({"profile":"ok","prompt":"x","output_schema":{"$ref":"https://example.invalid/x"}})).is_err());
    register(
        json!({"profile":"ok","prompt":"x","output_schema":schema,"require":["structured_output"]}),
    )
    .unwrap();
    assert_eq!(launches(home.path()), "0");
}

#[test]
fn workflow_steps_render_prior_outputs_into_harness_inputs_but_never_choose_the_executable() {
    let (_home, core) = fixture();
    let step = |profile: Value| {
        json!({"type":"workflow","input":{"params":{"who":"x"},"steps":[
        {"id":"a","action":{"type":"echo","input":{"text":"seed"}}},
        {"id":"h","needs":["a"],"action":{"type":"harness","input":{"profile":profile,"prompt":"Use ${/steps/a/output/text}","inputs":{"seed":{"$ref":"/steps/a/output/text"}},"output_schema":{"type":"object","properties":{"x":{"$ref":"#/$defs/x"}},"$defs":{"x":{"type":"string"}}}}}}]}})
    };
    core.register("operator", "chain", step(json!("ok")))
        .unwrap();
    // A templated profile is allowed only as a name and is resolved/bound at dispatch.
    core.register("operator", "dynamic", step(json!({"$ref":"/params/who"})))
        .unwrap();
    let rendered = crate::flow_schema::render_input(
        "harness",
        &step(json!("ok"))["input"]["steps"][1]["action"]["input"],
        &json!({"params":{},"steps":{"a":{"output":{"text":"seed"}}}}),
    )
    .unwrap();
    assert_eq!(rendered["inputs"]["seed"], "seed");
    assert_eq!(rendered["prompt"], "Use seed");
    assert_eq!(
        rendered["output_schema"]["properties"]["x"]["$ref"],
        "#/$defs/x"
    );
    assert!(core.validate_leaf("operator", &json!({"type":"harness","input":{"profile":"/bin/sh","prompt":"x","output_schema":{"type":"object"}}}), "execute").is_err());
}

#[tokio::test]
async fn unavailable_harness_is_refused_before_any_intent_is_recorded() {
    let (home, core) = fixture();
    let mut policy_profile = profile(home.path(), "true", "read-only", 5);
    policy_profile.executable = "/nonexistent/forge-harness".into();
    // Build a second core whose only runner points at a missing executable.
    let mut policy = core.policy.clone();
    policy.runners.insert("gone".into(), policy_profile);
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = core.store.clone();
    let core = Core::new(
        crate::settings::Paths::defaults(home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap();
    let e = admitted(&core, "gone");
    assert!(core.flow_effect(&e, "root", &action("gone")).await.is_err());
    assert!(
        core.flow_effect_observations(&e).unwrap().is_empty(),
        "no uncertain effect may exist"
    );
    assert!(core.harness_attempt(&e.dbos_id, "root").is_none());
    assert_eq!(launches(home.path()), "0");
}

#[tokio::test]
async fn cached_available_runner_cannot_authorize_a_missing_executable() {
    let (home, core) = fixture();
    let executable = home.path().join("removed-harness");
    std::fs::copy("/bin/sh", &executable).unwrap();
    let mut runner = profile(home.path(), "true", "read-only", 5);
    runner.executable = executable.to_str().unwrap().into();
    let mut policy = core.policy.clone();
    policy.runners.insert("removed".into(), runner);
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let core = Core::new(
        crate::settings::Paths::defaults(home.path()),
        policy,
        config,
        core.store.clone(),
        BTreeMap::new(),
    )
    .unwrap();
    assert!(core.probe_runner("removed").await.unwrap().available);
    let e = admitted(&core, "removed");
    std::fs::remove_file(&executable).unwrap();
    assert!(
        core.flow_effect(&e, "root", &action("removed"))
            .await
            .is_err()
    );
    assert!(core.flow_effect_observations(&e).unwrap().is_empty());
    assert!(core.harness_attempt(&e.dbos_id, "root").is_none());
    assert_eq!(launches(home.path()), "0");
    assert!(!core.probe_runner("removed").await.unwrap().available);
}

#[test]
fn unresolved_orphan_termination_refuses_startup_and_remains_retryable() {
    let (home, core) = fixture();
    let invocation = Core::harness_invocation("run-survivor", "root", 1);
    let dir = core.harness_dir(&invocation);
    crate::safety::private_mkdir(&home.path().join("state/harness")).unwrap();
    crate::safety::private_mkdir(&dir).unwrap();
    // Deterministic cgroup fault: writing the kill request succeeds, but membership
    // remains nonempty. No real process is signalled by this fixture.
    let group = home.path().join("unsettled-cgroup");
    std::fs::create_dir(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), "999999999\n").unwrap();
    let attempt = json!({"invocation_id":invocation,"run_id":"run-survivor",
        "state":"running","boot_id":crate::containment::boot_id(),"cgroup":group});
    std::fs::write(dir.join("attempt.json"), attempt.to_string()).unwrap();
    assert!(core.reconcile_harness_attempts().is_err());
    let stored: Value =
        serde_json::from_slice(&std::fs::read(dir.join("attempt.json")).unwrap()).unwrap();
    assert_eq!(stored["state"], "orphan_unresolved");
    assert!(
        stored["recovery"]
            .as_str()
            .unwrap()
            .contains("startup refused")
    );
    assert!(
        group.exists(),
        "unresolved containment evidence must remain"
    );
    std::fs::remove_file(group.join("cgroup.procs")).unwrap();
    std::fs::create_dir(group.join("cgroup.procs")).unwrap();
    assert!(
        core.reconcile_harness_attempts().is_err(),
        "unreadable membership is not proof of termination"
    );
    std::fs::remove_dir(group.join("cgroup.procs")).unwrap();
    std::fs::remove_file(group.join("cgroup.kill")).unwrap();
    // A real cgroup virtual `cgroup.procs` does not block leaf removal; once
    // this unreadable fixture is gone, retrying can prove absence and remove it.
    let settled = core.reconcile_harness_attempts().unwrap();
    assert_eq!(settled[0]["state"], "process_lost");
    assert!(core.reconcile_harness_attempts().unwrap().is_empty());
}

#[test]
fn recovery_refuses_unreadable_or_malformed_attempts_but_allows_prelaunch_absence() {
    let (home, core) = fixture();
    assert!(core.reconcile_harness_attempts().unwrap().is_empty());
    let root = home.path().join("state/harness");
    std::fs::write(&root, "not a directory").unwrap();
    assert!(core.reconcile_harness_attempts().is_err());
    std::fs::remove_file(&root).unwrap();
    crate::safety::private_mkdir(&root).unwrap();
    let dir = root.join("prepared");
    crate::safety::private_mkdir(&dir).unwrap();
    assert!(core.reconcile_harness_attempts().unwrap().is_empty());
    let record = dir.join("attempt.json");
    std::fs::create_dir(&record).unwrap();
    assert!(core.reconcile_harness_attempts().is_err());
    std::fs::remove_dir(&record).unwrap();
    std::fs::write(&record, "{broken").unwrap();
    assert!(core.reconcile_harness_attempts().is_err());
    assert_eq!(std::fs::read_to_string(&record).unwrap(), "{broken");
    for invalid in ["[]", "{}", "{\"state\":\"running\"}"] {
        std::fs::write(&record, invalid).unwrap();
        assert!(core.reconcile_harness_attempts().is_err());
    }
}

// ---- ACP and Pi transports: test-only fixture agents (mock supplements, never qualification) ----

/// Absolute path of a Python 3 interpreter found through the environment, never a literal.
fn python() -> Option<std::path::PathBuf> {
    std::env::var_os("FORGE_TEST_PYTHON")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|d| d.join("python3"))
                .find(|p| p.is_file())
        })
}

const ACP_FIXTURE: &str = r##"
import sys, json, os, pathlib
if sys.argv[1:] == ['--version']:
    print('acp-fixture'); sys.exit(0)
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
case = (root / 'case').read_text().strip()
count = root / 'count'
count.write_text(str(int(count.read_text()) + 1) if count.exists() else '1')
(root / 'pid').write_text(str(os.getpid()))
def send(m): sys.stdout.write(json.dumps(m) + '\n'); sys.stdout.flush()
def note(name, value):
    with open(root / name, 'a') as f: f.write(json.dumps(value) + '\n')
nid = [100]
def call(method, params):
    nid[0] += 1; send({'jsonrpc':'2.0','id':nid[0],'method':method,'params':params})
    while True:
        line = sys.stdin.readline()
        if not line: sys.exit(9)
        m = json.loads(line)
        if m.get('id') == nid[0] and 'method' not in m: return m
        if m.get('method') == 'session/cancel': note('cancel', m['params']); sys.exit(0)
def update(sid, u): send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':u}})
def chunk(sid, t): update(sid, {'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':t}})
for line in sys.stdin:
    m = json.loads(line)
    method, rid = m.get('method'), m.get('id')
    if method == 'initialize':
        note('init', m['params'])
        send({'jsonrpc':'2.0','id':rid,'result':{'protocolVersion':1,'agentInfo':{'name':'acp-fixture','version':'0'},'agentCapabilities':{'loadSession':False}}})
    elif method == 'session/new':
        note('new', m['params'])
        send({'jsonrpc':'2.0','id':rid,'result':{'sessionId':'fx-session'}})
    elif method == 'session/cancel':
        note('cancel', m['params']); sys.exit(0)
    elif method == 'session/prompt':
        sid = m['params']['sessionId']
        cwd = pathlib.Path(json.loads((root/'new').read_text().splitlines()[0])['cwd'])
        if case == 'slow':
            chunk(sid, 'working...')
            for l in sys.stdin:
                if json.loads(l).get('method') == 'session/cancel': note('cancel', {})
            sys.exit(0)
        if case == 'crash':
            chunk(sid, 'partial'); os._exit(7)
        if case == 'client_request':
            r = call('fs/read_text_file', {'sessionId':sid,'path':str(cwd/'inputs.json')})
            note('client_request_reply', r)
        chunk(sid, 'Thinking about it. ')
        outside = str(root / 'outside.txt')
        target = str(cwd / 'note.txt')
        if case in ('permission', 'permission_ro'):
            update(sid, {'sessionUpdate':'tool_call','toolCallId':'t1','title':'run shell','kind':'execute','status':'pending'})
            r = call('session/request_permission', {'sessionId':sid,'toolCall':{'toolCallId':'t1','title':'rm -rf','kind':'execute','locations':[]},'options':[{'optionId':'a','name':'Always','kind':'allow_always'},{'optionId':'y','name':'Allow','kind':'allow_once'},{'optionId':'n','name':'Reject','kind':'reject_once'}]})
            note('permission', r['result'])
            if r['result']['outcome'].get('optionId') in ('y','a'):
                (root / 'executed').write_text('yes')
            r = call('session/request_permission', {'sessionId':sid,'toolCall':{'toolCallId':'t2','title':'write','kind':'edit','locations':[{'path':outside}]},'options':[{'optionId':'y','name':'Allow','kind':'allow_once'},{'optionId':'n','name':'Reject','kind':'reject_once'}]})
            note('permission', r['result'])
            r = call('session/request_permission', {'sessionId':sid,'toolCall':{'toolCallId':'t3','title':'write note','kind':'edit','locations':[{'path':target}]},'options':[{'optionId':'y','name':'Allow','kind':'allow_once'},{'optionId':'n','name':'Reject','kind':'reject_once'}]})
            note('permission', r['result'])
            if r['result']['outcome'].get('optionId') == 'y':
                pathlib.Path(target).write_text('hello\n')
        else:
            pathlib.Path(target).write_text('hello\n')
            update(sid, {'sessionUpdate':'tool_call','toolCallId':'t1','title':'write note','kind':'edit','status':'in_progress'})
            update(sid, {'sessionUpdate':'tool_call_update','toolCallId':'t1','status':'completed'})
        chunk(sid, '{"answer":"42"}')
        send({'jsonrpc':'2.0','id':rid,'result':{'stopReason':'end_turn'}})
"##;

fn acp_fixture(case: &str, mode: &str, timeout: u64) -> Option<(tempfile::TempDir, Arc<Core>)> {
    let python = python()?;
    let (home, core) = fixture();
    let script = home.path().join("acp-fixture.py");
    std::fs::write(&script, ACP_FIXTURE).unwrap();
    std::fs::write(home.path().join("case"), case).unwrap();
    let mut policy = core.policy.clone();
    policy.runners.insert(
        "acp".into(),
        serde_json::from_value(json!({
            "adapter":"acp","executable":python,"args":[script],"permission_mode":mode,
            "timeout_seconds":timeout,"env":{"FIXTURE_ROOT":home.path()},"containment":"scan"
        }))
        .unwrap(),
    );
    let core = rebuild(&home, policy);
    Some((home, core))
}
fn rebuild(home: &tempfile::TempDir, policy: core::Policy) -> Arc<Core> {
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(
            &crate::settings::Paths::defaults(home.path()),
            &policy.home_id,
        )
        .unwrap(),
    );
    Core::new(
        crate::settings::Paths::defaults(home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap()
}
fn notes(home: &std::path::Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(home.join(name))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn acp_action(permissions: &str) -> Value {
    let mut a = action("acp");
    a["input"]["permissions"] = json!(permissions);
    a
}
fn attempt_dir(core: &Core, e: &Envelope, slot: &str) -> std::path::PathBuf {
    core.harness_dir(&Core::harness_invocation(&e.dbos_id, slot, 1))
}

#[tokio::test]
async fn acp_turn_streams_updates_records_a_validated_receipt_and_never_relaunches() {
    let Some((home, core)) = acp_fixture("ok", "workspace-write", 10) else {
        return;
    };
    let e = admitted(&core, "acp-ok");
    let a = acp_action("workspace-write");
    let first = core.flow_effect(&e, "root/step:h", &a).await.unwrap();
    assert_eq!(first["result"]["answer"], "42");
    assert_eq!(first["harness"]["transport"], "acp");
    assert_eq!(first["harness"]["tool_events"], 1);
    // Tool kind and the last reported status come from the session updates.
    let tool = &first["harness"]["tool_calls"][0];
    assert_eq!(
        (
            tool["id"].as_str(),
            tool["kind"].as_str(),
            tool["status"].as_str(),
            tool["title"].as_str()
        ),
        (
            Some("t1"),
            Some("edit"),
            Some("completed"),
            Some("write note")
        )
    );
    assert_eq!(first["harness"]["session_id"], "fx-session");
    assert_eq!(first["artifacts"][0]["text"], "hello\n");
    // The client advertises no fs/terminal capability and no MCP servers, and binds cwd to the workspace.
    let init = &notes(home.path(), "init")[0];
    assert_eq!(init["clientCapabilities"]["fs"]["readTextFile"], false);
    assert_eq!(init["clientCapabilities"]["terminal"], false);
    let new = &notes(home.path(), "new")[0];
    assert_eq!(new["mcpServers"], json!([]));
    assert!(new["cwd"].as_str().unwrap().ends_with("/workspace"));
    // Streamed updates are retained privately.
    let log = std::fs::read_to_string(attempt_dir(&core, &e, "root/step:h").join("updates.jsonl"))
        .unwrap();
    assert!(log.contains("agent_message_chunk") && log.contains("\"tool_call\""));
    let again = core.flow_effect(&e, "root/step:h", &a).await.unwrap();
    assert_eq!(again, first);
    assert_eq!(launches(home.path()), "1", "replay must reuse the receipt");
    let observed = core.flow_effect_observations(&e).unwrap();
    assert_eq!(observed[0]["state"], "completed");
    assert_eq!(observed[0]["harness_attempt"]["transport"], "acp");
    assert_eq!(observed[0]["harness_attempt"]["state"], "turn_completed");
}

#[tokio::test]
async fn acp_permission_requests_are_answered_by_policy_and_denials_do_not_execute() {
    for (mode, writes) in [("workspace-write", true), ("read-only", false)] {
        let Some((home, core)) = acp_fixture("permission", mode, 10) else {
            return;
        };
        let e = admitted(&core, "acp-perm");
        let out = core
            .flow_effect(&e, "root/step:h", &acp_action(mode))
            .await
            .unwrap();
        let replies = notes(home.path(), "permission");
        // Shell execution: rejected once (never the allow_always option); outside edit: rejected.
        assert_eq!(
            replies[0]["outcome"],
            json!({"outcome":"selected","optionId":"n"})
        );
        assert_eq!(
            replies[1]["outcome"],
            json!({"outcome":"selected","optionId":"n"})
        );
        assert!(!home.path().join("executed").exists());
        assert!(!home.path().join("outside.txt").exists());
        // The workspace edit is allowed once only under the workspace-write ceiling.
        let expected = if writes { "y" } else { "n" };
        assert_eq!(replies[2]["outcome"]["optionId"], expected);
        let ws = attempt_dir(&core, &e, "root/step:h").join("workspace/note.txt");
        assert_eq!(ws.exists(), writes);
        let decisions = out["harness"]["permission_decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), 3);
        assert_eq!(decisions[0]["decision"], "denied");
        assert_eq!(
            decisions[2]["decision"],
            if writes { "allowed_once" } else { "denied" }
        );
        let attempt = core.harness_attempt(&e.dbos_id, "root/step:h").unwrap();
        assert_eq!(attempt["permission_decisions"].as_array().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn acp_unknown_client_requests_are_refused_not_left_hanging() {
    let Some((home, core)) = acp_fixture("client_request", "read-only", 10) else {
        return;
    };
    let e = admitted(&core, "acp-client");
    core.flow_effect(&e, "root/step:h", &acp_action("read-only"))
        .await
        .unwrap();
    let reply = &notes(home.path(), "client_request_reply")[0];
    assert_eq!(reply["error"]["code"], -32601);
}

#[tokio::test]
async fn acp_timeout_cancels_the_turn_kills_the_agent_and_stays_uncertain() {
    let Some((home, core)) = acp_fixture("slow", "read-only", 1) else {
        return;
    };
    let e = admitted(&core, "acp-slow");
    let started = std::time::Instant::now();
    assert!(
        core.flow_effect(&e, "root/step:h", &acp_action("read-only"))
            .await
            .is_err()
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let pid: i32 = std::fs::read_to_string(home.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!alive(pid), "agent process must be terminated");
    assert_eq!(
        notes(home.path(), "cancel").len(),
        1,
        "session/cancel was sent before the kill"
    );
    let effects = core.flow_effect_observations(&e).unwrap();
    assert_eq!(effects[0]["state"], "uncertain");
    assert_eq!(effects[0]["harness_attempt"]["state"], "timed_out");
    assert!(
        core.flow_effect(&e, "root/step:h", &acp_action("read-only"))
            .await
            .is_err()
    );
    assert_eq!(launches(home.path()), "1", "no replacement launch");
}

#[tokio::test]
async fn acp_agent_crash_mid_turn_is_uncertain_and_not_replayed() {
    let Some((home, core)) = acp_fixture("crash", "read-only", 10) else {
        return;
    };
    let e = admitted(&core, "acp-crash");
    assert!(
        core.flow_effect(&e, "root/step:h", &acp_action("read-only"))
            .await
            .is_err()
    );
    assert!(
        core.flow_effect(&e, "root/step:h", &acp_action("read-only"))
            .await
            .is_err()
    );
    assert_eq!(launches(home.path()), "1");
    let effects = core.flow_effect_observations(&e).unwrap();
    assert_eq!(effects[0]["state"], "uncertain");
    let state = effects[0]["harness_attempt"]["state"].as_str().unwrap();
    assert!(matches!(state, "exited" | "protocol_error"), "{state}");
    assert_eq!(effects[0]["harness_attempt"]["result"], "rejected");
}

#[tokio::test]
async fn acp_cancellation_fence_cancels_the_turn_and_requires_reconciliation() {
    let Some((home, core)) = acp_fixture("slow", "read-only", 30) else {
        return;
    };
    let e = admitted(&core, "acp-fence");
    let runner = {
        let (core, e) = (core.clone(), e.clone());
        tokio::spawn(async move {
            core.flow_effect(&e, "root/step:h", &acp_action("read-only"))
                .await
        })
    };
    for _ in 0..100 {
        if home.path().join("pid").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    core.fence_flow(&e).unwrap();
    assert!(runner.await.unwrap().is_err());
    assert_eq!(notes(home.path(), "cancel").len(), 1);
    let effects = core.flow_effect_observations(&e).unwrap();
    assert_eq!(effects[0]["state"], "uncertain");
    assert_eq!(effects[0]["harness_attempt"]["state"], "cancelled");
}

#[test]
fn acp_and_pi_profiles_are_validated_and_cannot_widen_authority() {
    let ok = |v: Value| {
        serde_json::from_value::<RunnerProfile>(v)
            .unwrap()
            .validate("p")
    };
    let base = |adapter: &str| json!({"adapter":adapter,"executable":"/bin/true"});
    assert!(
        ok(base("acp")).is_err(),
        "generic acp requires its arguments"
    );
    let mut generic = base("acp");
    generic["args"] = json!(["--acp"]);
    assert!(ok(generic.clone()).is_ok());
    generic["model"] = json!("m");
    assert!(ok(generic).is_err(), "generic acp cannot select a model");
    for flag in [
        "--yolo",
        "--allow-all-tools",
        "--allow-tool=shell(*)",
        "--model=x",
    ] {
        let mut p = base("acp");
        p["args"] = json!([flag]);
        assert!(ok(p).is_err(), "{flag}");
    }
    for flag in [
        "--tools",
        "--tools=bash",
        "-t",
        "--extension",
        "--no-tools",
        "--model",
        "--session",
        "-a",
    ] {
        let mut p = base("pi");
        p["allow_unconfined"] = json!(true);
        p["args"] = json!([flag]);
        assert!(ok(p).is_err(), "{flag}");
    }
    let mut pi = base("pi");
    pi["allow_unconfined"] = json!(true);
    pi["provider"] = json!("local");
    assert!(ok(pi.clone()).is_err(), "provider without model");
    pi["model"] = json!("m");
    assert!(ok(pi).is_ok());
    let mut oc = base("opencode-acp");
    oc["env"] = json!({"OPENCODE_CONFIG_CONTENT":"{}"});
    assert!(ok(oc).is_err());
    let mut oc = base("opencode-acp");
    oc["args"] = json!(["--model", "x"]);
    assert!(ok(oc).is_err());
    // The soulsteads ACP variant shares the native contract's validation.
    let mut sn = base("soulsteads-acp");
    sn["model"] = json!("m");
    assert!(ok(sn.clone()).is_err(), "endpoint is mandatory");
    sn["env"] = json!({"HARNESS_BASE_URL":"http://localhost/v1"});
    assert!(ok(sn).is_ok());
}

#[test]
fn pi_is_gated_behind_acknowledgement_sandbox_and_an_isolated_home() {
    let ok = |v: Value| {
        serde_json::from_value::<RunnerProfile>(v)
            .unwrap()
            .validate("p")
    };
    let pi = || json!({"adapter":"pi","executable":"/bin/true","allow_unconfined":true});
    // Refused without the explicit acknowledgement, in every mode.
    for mode in ["read-only", "workspace-write"] {
        let unacknowledged =
            json!({"adapter":"pi","executable":"/bin/true","permission_mode":mode});
        let error = ok(unacknowledged).unwrap_err().to_string();
        assert!(error.contains("allow_unconfined"), "{error}");
    }
    assert!(ok(pi()).is_ok(), "acknowledged read-only is accepted");
    // workspace-write needs an OS sandbox.
    let mut write = pi();
    write["permission_mode"] = json!("workspace-write");
    assert!(ok(write.clone()).is_err());
    write["sandbox"] = json!({"kind":"bwrap","executable":"/usr/bin/bwrap"});
    assert!(ok(write.clone()).is_ok());
    for bad in [
        json!({"kind":"firejail","executable":"/usr/bin/firejail"}),
        json!({"kind":"bwrap","executable":"bwrap"}),
    ] {
        write["sandbox"] = bad;
        assert!(ok(write.clone()).is_err());
    }
    // The user HOME can never be supplied.
    let mut with_home = pi();
    with_home["env_passthrough"] = json!(["PATH", "HOME"]);
    assert!(ok(with_home).is_err());
    let mut with_home = pi();
    with_home["env"] = json!({"HOME":"/tmp/forge-fixture-home"});
    assert!(ok(with_home).is_err());
    // The acknowledgement and sandbox belong to pi only.
    let mut other = json!({"adapter":"codex","executable":"/bin/true","allow_unconfined":true});
    assert!(ok(other.clone()).is_err());
    other = json!({"adapter":"codex","executable":"/bin/true","sandbox":{"kind":"bwrap","executable":"/usr/bin/bwrap"}});
    assert!(ok(other).is_err());
    // Default passthrough for pi excludes HOME; other adapters keep it.
    let profile: RunnerProfile = serde_json::from_value(pi()).unwrap();
    assert!(!profile.passthrough().contains(&"HOME".to_owned()));
    // The capability matrix: pi lacks workspace_confinement, the ACP family has it.
    assert!(!profile.capabilities().contains(&"workspace_confinement"));
    let acp: RunnerProfile =
        serde_json::from_value(json!({"adapter":"opencode-acp","executable":"/bin/true"})).unwrap();
    assert!(acp.capabilities().contains(&"workspace_confinement"));
    // Unset flags do not change the serialized (and so admission-bound) profile.
    let encoded = serde_json::to_value(&acp).unwrap();
    assert!(encoded.get("allow_unconfined").is_none() && encoded.get("sandbox").is_none());
    // bwrap wiring: read-only root, hidden HOME, writable workspace and pi home only.
    let ws = std::path::Path::new("/w/ws");
    let argv = crate::harness::pi_bwrap_argv(
        "/opt/pi/pi",
        ws,
        std::path::Path::new("/w/pi-home"),
        Some(std::path::Path::new("/keys")),
        &["--mode".into(), "rpc".into()],
    );
    let joined = argv.join(" ");
    assert!(joined.starts_with("--die-with-parent --new-session --ro-bind / /"));
    assert!(
        joined.contains("--bind /w/ws /w/ws") && joined.contains("--bind /w/pi-home /w/pi-home")
    );
    assert!(joined.contains("--bind /keys /keys") && joined.contains("--ro-bind /opt/pi /opt/pi"));
    assert!(joined.ends_with("--chdir /w/ws -- /opt/pi/pi --mode rpc"));
    // The session and system bus runtime directories are hidden (those that exist).
    for dir in crate::harness::PI_HIDDEN_RUNTIME_DIRS {
        let hidden = argv.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == *dir);
        assert_eq!(hidden, std::path::Path::new(dir).is_dir(), "{dir}");
    }
}

#[test]
fn the_pi_agent_directory_must_be_a_dedicated_existing_directory() {
    let home = tempfile::tempdir().unwrap();
    let forge_home = home.path().join("forge-home");
    std::fs::create_dir_all(forge_home.join("state/outputs")).unwrap();
    let paths = crate::settings::Paths::defaults(&forge_home);
    let dedicated = home.path().join("pi-agent");
    std::fs::create_dir(&dedicated).unwrap();
    let file = home.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let link = home.path().join("a-link");
    std::os::unix::fs::symlink(&dedicated, &link).unwrap();
    let user_home = std::env::var("HOME").unwrap_or_default();
    let profile = |dir: &std::path::Path| -> RunnerProfile {
        serde_json::from_value(json!({
            "adapter":"pi","executable":"/bin/true","allow_unconfined":true,
            "env":{"PI_CODING_AGENT_DIR":dir}
        }))
        .unwrap()
    };
    let refused = |dir: &std::path::Path| {
        profile(dir)
            .validate_for_paths("p", &paths)
            .unwrap_err()
            .to_string()
    };
    assert!(profile(&dedicated).validate_for_paths("p", &paths).is_ok());
    // Not a plain existing directory, not absolute, no `..`.
    for bad in [home.path().join("missing"), file, link] {
        assert!(refused(&bad).contains("PI_CODING_AGENT_DIR"), "{bad:?}");
    }
    assert!(refused(std::path::Path::new("relative/dir")).contains("absolute"));
    assert!(refused(&dedicated.join("../pi-agent")).contains("absolute path without `..`"));
    // Not / , not HOME, not the Forge home or any Forge path, and not an ancestor of one.
    assert!(refused(std::path::Path::new("/")).contains("root"));
    if !user_home.is_empty() {
        assert!(refused(std::path::Path::new(&user_home)).contains("HOME"));
    }
    for dir in [
        forge_home.clone(),
        forge_home.join("state"),
        forge_home.join("state/outputs"),
        home.path().to_path_buf(),
    ] {
        assert!(
            refused(&dir).contains("separate from the Forge home"),
            "{dir:?}"
        );
    }
    // A settings-relocated path is protected as well.
    let moved = home.path().join("elsewhere-state");
    std::fs::create_dir(&moved).unwrap();
    let relocated = crate::settings::Paths::resolve(
        &forge_home,
        &crate::settings::PathsConfig {
            state: Some(moved.clone()),
            ..Default::default()
        },
        &|_| None,
    )
    .unwrap();
    assert!(profile(&moved).validate_for_paths("p", &relocated).is_err());
    // Plain validation is unchanged for other adapters and for profiles without the variable.
    let plain: RunnerProfile = serde_json::from_value(
        json!({"adapter":"pi","executable":"/bin/true","allow_unconfined":true}),
    )
    .unwrap();
    assert!(plain.validate_for_paths("p", &paths).is_ok());
}

#[tokio::test]
async fn the_sandbox_executable_must_report_bubblewrap() {
    let home = tempfile::tempdir().unwrap();
    let script = |name: &str, body: &str| {
        let path = home.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        path
    };
    let probe_with = |sandbox: &std::path::Path| {
        let profile: RunnerProfile = serde_json::from_value(json!({
            "adapter":"pi","executable":"/bin/true","allow_unconfined":true,
            "sandbox":{"kind":"bwrap","executable":sandbox}
        }))
        .unwrap();
        async move { crate::harness::probe(&profile).await }
    };
    let imposter = script("not-bwrap", "echo 'definitely something else 1.0'");
    let failing = script("fails", "exit 3");
    let genuine = script("real-ish", "echo 'bubblewrap 0.9.0'");
    for bad in [&imposter, &failing, &home.path().join("missing")] {
        let probe = probe_with(bad).await;
        assert!(!probe.available, "{bad:?}");
        assert!(probe.reason.unwrap().contains("not bubblewrap"));
    }
    assert!(probe_with(&genuine).await.available);
}

const PI_FIXTURE: &str = r##"
import sys, json, os, pathlib
if sys.argv[1:] == ['--version']:
    print('0.0.0-fixture'); sys.exit(0)
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
(root / 'argv').write_text(json.dumps(sys.argv[1:]))
(root / 'pi_home').write_text(os.environ.get('HOME', ''))
count = root / 'count'
count.write_text(str(int(count.read_text()) + 1) if count.exists() else '1')
def send(m): sys.stdout.write(json.dumps(m) + '\n'); sys.stdout.flush()
print('pi banner that is not protocol')
for line in sys.stdin:
    m = json.loads(line)
    if m['type'] != 'prompt': continue
    send({'id':m['id'],'type':'response','command':'prompt','success':True,'data':{'disposition':'started'}})
    send({'type':'agent_start'})
    send({'type':'message_start','message':{'role':'assistant'}})
    for part in ['{"answer"', ':"42"}']:
        send({'type':'message_update','assistantMessageEvent':{'type':'text_delta','delta':part,'contentIndex':0}})
    pathlib.Path('note.txt').write_text('hello\n')
    send({'type':'tool_execution_start','toolCallId':'c1','toolName':'write','args':{}})
    send({'type':'tool_execution_end','toolCallId':'c1','toolName':'write','result':{},'isError':False})
    send({'type':'message_end','message':{'role':'assistant','stopReason':'stop','model':'fixture-model','content':[{'type':'text','text':'{"answer":"42"}'}]}})
    send({'type':'agent_end','messages':[],'willRetry':False})
    send({'type':'agent_settled','aborted':False})
"##;

#[tokio::test]
async fn pi_rpc_turn_uses_the_derived_launch_arguments_and_parses_events() {
    let Some(python) = python() else {
        return;
    };
    let (home, core) = fixture();
    let script = home.path().join("pi-fixture");
    std::fs::write(&script, format!("#!{}\n{PI_FIXTURE}", python.display())).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    // A stand-in for bwrap: records its arguments, then runs the command after `--`.
    let sandbox = home.path().join("fake-bwrap");
    std::fs::write(
        &sandbox,
        format!(
            "#!{}\nimport os, sys, json\nargs = sys.argv[1:]\nif args == ['--version']:\n    print('bubblewrap 0.0-fixture'); sys.exit(0)\nopen(os.environ['FIXTURE_ROOT'] + '/sandbox_args', 'w').write(json.dumps(args))\ni = args.index('--')\nos.chdir(args[args.index('--chdir') + 1])\nos.execv(args[i + 1], args[i + 1:])\n",
            python.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &sandbox,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let mut policy = core.policy.clone();
    policy.runners.insert(
        "pi".into(),
        serde_json::from_value(json!({
            "adapter":"pi","executable":script,"provider":"local","model":"fixture-model",
            "permission_mode":"workspace-write","timeout_seconds":10,"env":{"FIXTURE_ROOT":home.path()},"containment":"scan",
            "allow_unconfined":true,"sandbox":{"kind":"bwrap","executable":sandbox}
        }))
        .unwrap(),
    );
    let core = rebuild(&home, policy);
    let e = admitted(&core, "pi-ok");
    let out = core
        .flow_effect(&e, "root/step:h", &{
            let mut a = action("pi");
            a["input"]["permissions"] = json!("workspace-write");
            a
        })
        .await
        .unwrap();
    assert_eq!(out["result"]["answer"], "42");
    assert_eq!(out["harness"]["transport"], "pi-rpc");
    assert_eq!(out["harness"]["served_model"], "fixture-model");
    assert_eq!(out["harness"]["tool_events"], 1);
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(home.path().join("argv")).unwrap()).unwrap();
    assert!(argv.windows(2).any(|w| w == ["--mode", "rpc"]));
    assert!(
        argv.windows(2)
            .any(|w| w == ["--tools", "read,grep,find,ls,edit,write"])
    );
    assert!(argv.windows(2).any(|w| w == ["--model", "fixture-model"]));
    assert_eq!(out["artifacts"][0]["text"], "hello\n");
    // Pi ran through the sandbox launcher with an isolated HOME, never the user's.
    let pi_home = std::fs::read_to_string(home.path().join("pi_home")).unwrap();
    assert!(pi_home.ends_with("/pi-home"), "{pi_home}");
    assert_ne!(
        Some(pi_home.as_str()),
        std::env::var("HOME").ok().as_deref()
    );
    assert!(std::path::Path::new(&pi_home).starts_with(core.paths.harness.as_path()));
    let sandbox_args: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(home.path().join("sandbox_args")).unwrap())
            .unwrap();
    assert!(
        sandbox_args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1].ends_with("/workspace") && w[1] == w[2])
    );
    assert!(
        sandbox_args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == pi_home && w[2] == pi_home)
    );
}

#[tokio::test]
async fn pi_cannot_satisfy_a_workspace_confinement_requirement() {
    let (home, core) = fixture();
    let mut policy = core.policy.clone();
    policy.runners.insert(
        "pi".into(),
        serde_json::from_value(
            json!({"adapter":"pi","executable":"/bin/true","allow_unconfined":true}),
        )
        .unwrap(),
    );
    let core = rebuild(&home, policy);
    let mut with_requirement = action("pi");
    with_requirement["input"]["require"] = json!(["workspace_confinement"]);
    assert!(core.validate_harness(&with_requirement["input"]).is_err());
    assert!(core.validate_harness(&action("pi")["input"]).is_ok());
}

#[tokio::test]
async fn acp_run_interrupted_by_forge_loss_is_recovery_required_and_never_blindly_replayed() {
    let Some((home, core)) = acp_fixture("slow", "read-only", 60) else {
        return;
    };
    let e = admitted(&core, "acp-loss");
    let slot = "root/step:h";
    // The effect intent is durable (`uncertain`) before the agent launches, so a loss of
    // Forge at any point after the intent leaves the run unresolved, never replayable.
    let runner = {
        let (core, e) = (core.clone(), e.clone());
        tokio::spawn(async move { core.flow_effect(&e, slot, &acp_action("read-only")).await })
    };
    for _ in 0..100 {
        if home.path().join("pid").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        core.harness_attempt(&e.dbos_id, slot).unwrap()["state"],
        "running"
    );
    assert_eq!(
        core.flow_effect_observations(&e).unwrap()[0]["state"],
        "uncertain"
    );
    // Forge dies: the in-flight call vanishes without recording any outcome.
    runner.abort();
    let _ = runner.await;
    let pid: i32 = std::fs::read_to_string(home.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Startup reconciliation terminates (never adopts) what survived and rewrites the attempt.
    let report = core.reconcile_harness_attempts().unwrap();
    assert!(!report.is_empty());
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!alive(pid), "no orphan ACP agent survives restart");
    let attempt = core.harness_attempt(&e.dbos_id, slot).unwrap();
    assert!(
        matches!(
            attempt["state"].as_str(),
            Some("orphan_terminated" | "process_lost")
        ),
        "{attempt}"
    );
    let effects = core.flow_effect_observations(&e).unwrap();
    assert_eq!(
        effects[0]["state"], "uncertain",
        "RECOVERY_REQUIRED, not completed"
    );
    // DBOS recovery re-enters the step: it must refuse, not relaunch a replacement session.
    assert!(
        core.flow_effect(&e, slot, &acp_action("read-only"))
            .await
            .is_err()
    );
    assert_eq!(launches(home.path()), "1");
}

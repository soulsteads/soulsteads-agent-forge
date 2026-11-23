//! Unprivileged containment of a harness and every descendant it creates.
//!
//! Preferred: a per-invocation cgroup v2 leaf under Forge's own (user-delegated)
//! cgroup. Membership is inherited across fork/setsid/double-fork, `cgroup.kill`
//! is atomic, and identity is the kernel's membership, never a pid number.
//! Fallback ("scan"): Forge becomes a child subreaper and finds descendants by
//! parent ancestry plus the `FORGE_INVOCATION_ID` environment marker. A process
//! that scrubs its own environment, detaches (so it is reparented to Forge) and
//! is not a descendant by ancestry can escape the fallback; that residual limit
//! is documented. The cgroup mode has no such escape for processes Forge can see.
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Cgroup,
    Scan,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestFailure {
    TerminationProof,
    LeafRemoval,
}

#[cfg(test)]
type Injected = std::collections::BTreeMap<String, TestFailure>;
#[cfg(test)]
static TEST_FAILURES: std::sync::OnceLock<std::sync::Mutex<Injected>> = std::sync::OnceLock::new();
/// Inject one failure for exactly one invocation. The failure is keyed by invocation
/// identity, so tests running concurrently in this process can never consume it.
#[cfg(test)]
pub(crate) fn set_test_failure(invocation: &str, failure: Option<TestFailure>) {
    let mut configured = TEST_FAILURES
        .get_or_init(|| std::sync::Mutex::new(Injected::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match failure {
        Some(failure) => configured.insert(invocation.to_owned(), failure),
        None => configured.remove(invocation),
    };
}
#[cfg(test)]
fn test_failure(invocation: &str, failure: TestFailure) -> Result<()> {
    let mut configured = TEST_FAILURES
        .get_or_init(|| std::sync::Mutex::new(Injected::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if configured.get(invocation) == Some(&failure) {
        configured.remove(invocation);
        anyhow::bail!("deterministic containment proof failure");
    }
    Ok(())
}

pub fn marker(invocation: &str) -> String {
    format!("FORGE_INVOCATION_ID={invocation}")
}

fn cgroup_base() -> Option<PathBuf> {
    let raw = fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = raw.lines().find_map(|l| l.strip_prefix("0::"))?;
    let path = crate::settings::cgroup_root().join(rel.trim_start_matches('/'));
    path.is_dir().then_some(path)
}
fn leaf_name(invocation: &str) -> String {
    format!("forge-{invocation}")
}

pub struct Contained {
    pub kind: &'static str,
    pub cgroup: Option<PathBuf>,
    procs_fd: Option<fs::File>,
    invocation: String,
    root: Option<u32>,
    armed: bool,
}

pub fn set_subreaper() -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("subreaper containment unavailable");
    }
    Ok(())
}

impl Contained {
    pub fn create(mode: Mode, invocation: &str) -> Result<Self> {
        if mode != Mode::Scan
            && let Some(base) = cgroup_base()
        {
            let leaf = base.join(leaf_name(invocation));
            let made = match fs::create_dir(&leaf) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    anyhow::bail!("unreconciled containment leaf already exists")
                }
                Err(_) => false,
            };
            let opened = if made {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_CLOEXEC)
                    .open(leaf.join("cgroup.procs"))
                    .ok()
            } else {
                None
            };
            if let Some(fd) = opened {
                return Ok(Self {
                    kind: "cgroup",
                    cgroup: Some(leaf),
                    procs_fd: Some(fd),
                    invocation: invocation.into(),
                    root: None,
                    armed: true,
                });
            }
            if made {
                fs::remove_dir(&leaf).context("unusable containment leaf could not be removed")?;
            }
        }
        if mode == Mode::Cgroup {
            anyhow::bail!("cgroup containment requested but unavailable");
        }
        set_subreaper()?;
        Ok(Self {
            kind: "scan",
            cgroup: None,
            procs_fd: None,
            invocation: invocation.into(),
            root: None,
            armed: true,
        })
    }
    /// Install the child's cgroup membership before exec (cgroup mode).
    pub fn prepare(&self, command: &mut std::process::Command) {
        if let Some(fd) = &self.procs_fd {
            use std::os::unix::process::CommandExt;
            let raw = fd.as_raw_fd();
            unsafe {
                command.pre_exec(move || {
                    let bytes = b"0";
                    if libc::write(raw, bytes.as_ptr().cast(), 1) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
    pub fn set_root(&mut self, pid: u32) {
        self.root = Some(pid);
    }
    pub fn disarm(&mut self) {
        self.armed = false;
    }
    /// Terminate every contained process and positively confirm the tree is gone.
    /// `root_reaped` means the direct child was already waited for, so its pid
    /// must not be signalled (pid-reuse guard).
    pub fn kill(&self, root_reaped: bool) -> Result<()> {
        terminate_invocation(
            &self.invocation,
            self.cgroup.as_deref(),
            self.root.filter(|_| !root_reaped),
            true,
        )
    }
    pub fn cleanup(&mut self) -> Result<()> {
        #[cfg(test)]
        test_failure(&self.invocation, TestFailure::LeafRemoval)?;
        self.procs_fd = None;
        if let Some(path) = &self.cgroup {
            remove_leaf(path)?;
            self.cgroup = None;
        }
        Ok(())
    }
}
impl Drop for Contained {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill_invocation(&self.invocation, self.cgroup.as_deref(), self.root);
            let _ = self.cleanup();
        }
    }
}

fn read_stat(pid: u32) -> Option<(u32, u64)> {
    read_stat_full(pid).map(|(_, p, t)| (p, t))
}
/// (state letter, ppid, start ticks)
fn read_stat_full(pid: u32) -> Option<(char, u32, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &raw[raw.rfind(')')? + 2..];
    let fields: Vec<&str> = after.split(' ').collect();
    // fields[0]=state, [1]=ppid, ..., [19]=starttime
    Some((
        fields.first()?.chars().next()?,
        fields.get(1)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
    ))
}
pub fn start_ticks(pid: u32) -> Option<u64> {
    read_stat(pid).map(|(_, t)| t)
}
pub fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default()
        .trim()
        .to_owned()
}
fn has_marker(pid: u32, marker: &str) -> bool {
    fs::read(format!("/proc/{pid}/environ"))
        .map(|bytes| bytes.split(|b| *b == 0).any(|v| v == marker.as_bytes()))
        .unwrap_or(false)
}
fn all_pids() -> Result<Vec<u32>> {
    Ok(fs::read_dir("/proc")?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect())
}
/// Descendants by ancestry from `root` plus marker-carrying processes.
fn scan(invocation: &str, root: Option<u32>) -> Result<BTreeSet<u32>> {
    let mark = marker(invocation);
    let me = std::process::id();
    let mut parents = BTreeMap::new();
    for pid in all_pids()? {
        // Zombies are already dead; they only await a wait() and must not be "alive".
        if let Some((state, ppid, _)) = read_stat_full(pid)
            && state != 'Z'
        {
            parents.insert(pid, ppid);
        }
    }
    let mut found = BTreeSet::new();
    if let Some(root) = root
        && parents.contains_key(&root)
    {
        found.insert(root);
    }
    loop {
        let before = found.len();
        for (pid, ppid) in &parents {
            if found.contains(ppid) {
                found.insert(*pid);
            }
        }
        if found.len() == before {
            break;
        }
    }
    for pid in parents.keys() {
        if !found.contains(pid) && has_marker(*pid, &mark) {
            found.insert(*pid);
        }
    }
    found.remove(&me);
    Ok(found)
}
fn cgroup_pids(path: &Path) -> Result<Vec<u32>> {
    Ok(fs::read_to_string(path.join("cgroup.procs"))?
        .lines()
        .map(|line| line.trim().parse())
        .collect::<std::result::Result<_, _>>()?)
}

fn kill_pid(pid: i32, context: &'static str) -> Result<()> {
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(std::io::Error::last_os_error()).context(context);
    }
    Ok(())
}

/// Kill everything attributable to one invocation and prove no live members
/// remain before returning. The scan fallback retains its documented limitation
/// for an environment-scrubbing, detached process outside the descendant tree.
pub fn kill_invocation(invocation: &str, cgroup: Option<&Path>, root: Option<u32>) -> Result<()> {
    terminate_invocation(invocation, cgroup, root, false)
}

fn terminate_invocation(
    invocation: &str,
    cgroup: Option<&Path>,
    root: Option<u32>,
    preserve_root_status: bool,
) -> Result<()> {
    if let Some(path) = cgroup {
        for _ in 0..40 {
            if cgroup_pids(path)?.is_empty() {
                #[cfg(test)]
                test_failure(invocation, TestFailure::TerminationProof)?;
                return Ok(());
            }
            if fs::write(path.join("cgroup.kill"), "1").is_err() {
                for pid in cgroup_pids(path)? {
                    kill_pid(pid as i32, "contained cgroup process could not be killed")?;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!("contained cgroup termination could not be confirmed");
    }
    if let Some(root) = root {
        kill_pid(
            -(root as i32),
            "contained process group could not be killed",
        )?;
    }
    for _ in 0..40 {
        let victims = scan(invocation, root)?;
        if victims.is_empty() {
            #[cfg(test)]
            test_failure(invocation, TestFailure::TerminationProof)?;
            return Ok(());
        }
        for pid in &victims {
            kill_pid(*pid as i32, "contained process could not be killed")?;
        }
        std::thread::sleep(Duration::from_millis(50));
        reap_scan_victims(victims, root.filter(|_| preserve_root_status));
    }
    anyhow::bail!("contained scan termination could not be confirmed")
}

fn reap_scan_victims(victims: BTreeSet<u32>, direct_child: Option<u32>) {
    for pid in victims {
        // The live caller must wait on its Child. If the scan caught the
        // root before it exited, reaping it here would make that wait fail.
        if Some(pid) == direct_child {
            continue;
        }
        // Reap reparented zombies that became our children; ECHILD otherwise.
        let mut status = 0;
        unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    }
}

/// Is anything of this invocation still alive? A filesystem/procfs read failure
/// is an inability to prove absence, not evidence that the tree is gone.
pub fn alive(
    invocation: &str,
    cgroup: Option<&Path>,
    identity: Option<(u32, u64)>,
) -> Result<bool> {
    if let Some(path) = cgroup {
        return match cgroup_pids(path) {
            Ok(pids) => Ok(!pids.is_empty()),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        };
    }
    let mut candidates = scan(invocation, None)?;
    if let Some((pid, ticks)) = identity
        && start_ticks(pid) == Some(ticks)
        && read_stat_full(pid).is_some_and(|(state, _, _)| state != 'Z')
    {
        candidates.insert(pid);
    }
    Ok(!candidates.is_empty())
}

/// Remove a cgroup leaf. A leaf that is already gone (for example in a restored home, or
/// after a reboot cleared the cgroup tree) is success; every other failure stays fatal.
pub fn remove_leaf(cgroup: &Path) -> Result<()> {
    match fs::remove_dir(cgroup) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other.context("cgroup leaf could not be removed"),
    }
}

/// Recover a prior invocation from its durable identity. A cgroup leaf is
/// deterministic from that identity; if it no longer exists, fall back to the
/// subreaper/marker scan used by scan-mode containment.
pub fn reconcile_invocation(invocation: &str) -> Result<()> {
    set_subreaper()?;
    let cgroup = cgroup_base()
        .map(|base| base.join(leaf_name(invocation)))
        .filter(|path| path.exists());
    let cgroup_ref = cgroup.as_deref();
    if alive(invocation, cgroup_ref, None)? {
        kill_invocation(invocation, cgroup_ref, None)?;
    }
    if alive(invocation, cgroup_ref, None)? {
        anyhow::bail!("contained invocation termination could not be confirmed");
    }
    if let Some(path) = cgroup {
        remove_leaf(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{read_stat_full, reap_scan_victims, remove_leaf};
    use std::time::{Duration, Instant};
    use std::{os::unix::process::ExitStatusExt, process::Command};

    #[test]
    fn scan_termination_leaves_the_direct_child_for_its_owner_to_reap() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        // Model a snapshot that found the root alive, followed by its exit
        // before the reap pass. Observe the zombie without consuming its status.
        child.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !read_stat_full(pid).is_some_and(|(state, _, _)| state == 'Z') {
            if Instant::now() >= deadline {
                let _ = child.wait();
                panic!("direct child did not become waitable");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        reap_scan_victims([pid].into_iter().collect(), Some(pid));
        assert_eq!(
            child
                .wait()
                .expect("containment must not consume the direct child exit status")
                .signal(),
            Some(libc::SIGKILL)
        );
    }

    #[test]
    fn a_missing_leaf_is_already_removed_but_other_failures_stay_fatal() {
        let dir = tempfile::tempdir().unwrap();
        remove_leaf(&dir.path().join("gone")).unwrap();
        let leaf = dir.path().join("leaf");
        std::fs::create_dir(&leaf).unwrap();
        remove_leaf(&leaf).unwrap();
        assert!(!leaf.exists());
        // A non-empty directory cannot be removed: still an error.
        let busy = dir.path().join("busy");
        std::fs::create_dir(&busy).unwrap();
        std::fs::write(busy.join("f"), b"x").unwrap();
        assert!(remove_leaf(&busy).is_err());
        // A file where a directory is expected is an error, not "gone".
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(remove_leaf(&file).is_err());
    }
}

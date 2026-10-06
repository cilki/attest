use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, trace, warn};

static COUNTER: AtomicU64 = AtomicU64::new(0);
/// Resolved once per process: the `/sys/fs/cgroup/.../attest` directory that
/// belongs to this user. `None` if cgroups are unavailable or unwritable.
static ATTEST_BASE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Resource usage captured from cgroup v2 for a single test run.
/// Fields are `None` when the corresponding controller is unavailable.
#[derive(Debug, Clone, Default)]
pub struct ResourceStats {
    pub cpu_user_usec: Option<u64>,
    pub cpu_system_usec: Option<u64>,
    pub memory_peak: Option<u64>,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    pub pids_peak: Option<u64>,
}

/// A cgroup directory created for a single test. The forked child calls
/// `enter()` to place itself inside it; the parent reads stats after the child
/// exits and the cgroup is cleaned up on drop.
pub struct TestCgroup {
    path: PathBuf,
}

impl TestCgroup {
    /// Attempt to create a per-test cgroup directory. Returns `None` when
    /// cgroups are unavailable or the process lacks permission.
    pub fn try_create(test_id: &str) -> Option<Self> {
        let base = ATTEST_BASE.get_or_init(init_base).as_ref()?;

        let safe_id: String = test_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("{safe_id}_{count}"));

        if let Err(e) = std::fs::create_dir(&path) {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                let _ = std::fs::remove_dir(&path);
                if let Err(e2) = std::fs::create_dir(&path) {
                    trace!("failed to create test cgroup: {e2}");
                    return None;
                }
            } else {
                trace!("failed to create test cgroup: {e}");
                return None;
            }
        }

        Some(Self { path })
    }

    /// The `cgroup.procs` path as a `CString`, ready for [`add_self_to`] in a
    /// `Command::pre_exec` hook (after fork, before exec).
    pub fn procs_cstring(&self) -> Option<CString> {
        CString::new(self.path.join("cgroup.procs").into_os_string().into_vec()).ok()
    }

    /// Kill every process in the cgroup by writing to `cgroup.kill`
    /// (Linux 5.14+). Best-effort: catches processes that escaped the test's
    /// process group (e.g. daemonized with their own setsid).
    pub fn kill_all(&self) {
        let _ = std::fs::write(self.path.join("cgroup.kill"), "1");
    }

    /// Read total CPU time (user + system) from the cgroup. Returns `None`
    /// when the cpu controller is unavailable.
    pub fn read_cpu_time(&self) -> Option<std::time::Duration> {
        let (user, system) = read_cpu_usec(self.path.join("cpu.stat"));
        Some(std::time::Duration::from_micros(
            user? + system.unwrap_or(0),
        ))
    }

    /// Read resource stats from the cgroup pseudo-files. Call this after the
    /// child has exited (waitpid returned) but before dropping the handle.
    pub fn read_stats(&self) -> ResourceStats {
        let (cpu_user_usec, cpu_system_usec) = read_cpu_usec(self.path.join("cpu.stat"));
        ResourceStats {
            cpu_user_usec,
            cpu_system_usec,
            memory_peak: read_single_u64(self.path.join("memory.peak")).or_else(|| {
                trace!("memory.peak unavailable, falling back to memory.current");
                read_single_u64(self.path.join("memory.current"))
            }),
            io_read_bytes: read_io_field(&self.path, "rbytes"),
            io_write_bytes: read_io_field(&self.path, "wbytes"),
            pids_peak: read_single_u64(self.path.join("pids.peak")),
        }
    }
}

impl Drop for TestCgroup {
    fn drop(&mut self) {
        // kill_all is asynchronous: members of the cgroup may still be dying
        // when we get here, and a cgroup directory cannot be removed while it
        // has members. Retry briefly before giving up.
        for _ in 0..50 {
            if std::fs::remove_dir(&self.path).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        if let Err(e) = std::fs::remove_dir(&self.path) {
            warn!("failed to remove test cgroup {:?}: {e}", self.path);
        }
    }
}

fn cgroup_type(path: &Path) -> String {
    std::fs::read_to_string(path.join("cgroup.type"))
        .unwrap_or_else(|_| "domain".to_string())
        .trim()
        .to_string()
}

fn is_domain(path: &Path) -> bool {
    cgroup_type(path) == "domain"
}

/// Ensure `path` is a usable "domain" cgroup, creating or recovering it as needed.
/// Returns false if the cgroup cannot be made usable.
fn ensure_domain_cgroup(path: &Path) -> bool {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !is_domain(path) {
                debug!(path=%path.display(), "stale cgroup (type='{}'): purging and recreating", cgroup_type(path));
                purge_cgroup_children(path);
                let _ = std::fs::remove_dir(path);
                if let Err(e2) = std::fs::create_dir(path) {
                    debug!(path=%path.display(), "recreate failed: {e2}");
                    return false;
                }
            }
        }
        Err(e) => {
            debug!(path=%path.display(), "create failed: {e}");
            return false;
        }
    }
    if !is_domain(path) {
        debug!(path=%path.display(), "newly created cgroup has unexpected type '{}'", cgroup_type(path));
        let _ = std::fs::remove_dir(path);
        return false;
    }
    true
}

/// Remove all empty child cgroup directories under `dir` (best-effort).
fn purge_cgroup_children(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            purge_cgroup_children(&p);
            let _ = std::fs::remove_dir(&p);
        }
    }
}

/// The process's own cgroup directory followed by each of its ancestors, up to
/// but not including the cgroup root (which can never parent an `attest` base
/// that holds processes).
fn cgroup_ancestors(leaf: PathBuf) -> impl Iterator<Item = PathBuf> {
    std::iter::successors(Some(leaf), |dir| {
        dir.parent()
            .filter(|p| p.starts_with("/sys/fs/cgroup") && *p != Path::new("/sys/fs/cgroup"))
            .map(Path::to_path_buf)
    })
}

/// Outcome of trying to build the `attest` base under one ancestor cgroup.
enum Attempt {
    /// A usable base cgroup, with the controllers we need enabled for its children.
    Usable(PathBuf),
    /// Nothing usable here, but an ancestor further up may still work.
    TryParent,
    /// Nothing in this hierarchy can work; stop walking.
    GiveUp,
}

/// Try to set up `<ancestor>/attest` as the base holding one cgroup per test,
/// leaving nothing behind unless it succeeds. `pid` is this process's pid as a
/// decimal string, ready to write to a `cgroup.procs`.
fn try_ancestor(ancestor: &Path, pid: &str) -> Attempt {
    // Only "domain" cgroups can parent child cgroups that hold processes.
    // "domain threaded" and "domain invalid" ancestors yield unusable children.
    if !is_domain(ancestor) {
        debug!(path=%ancestor.display(), "ancestor type is '{}'; skipping", cgroup_type(ancestor));
        return Attempt::TryParent;
    }

    let base = ancestor.join("attest");
    if !ensure_domain_cgroup(&base) {
        return Attempt::TryParent;
    }

    // cgroup v2 no-internal-process constraint: a non-root cgroup that has
    // child cgroups cannot directly contain processes. Move the current process
    // into base/main (a leaf) so that any child we fork also starts in a leaf
    // and can freely migrate to a sibling test cgroup via cgroup.procs.
    let main_cgroup = base.join("main");
    let attempt = if !ensure_domain_cgroup(&main_cgroup) {
        Attempt::TryParent
    } else if let Err(e) = std::fs::write(main_cgroup.join("cgroup.procs"), pid) {
        if e.raw_os_error() == Some(libc::EOPNOTSUPP) {
            // The cgroup is in a threaded subtree; cgroup.procs is not valid
            // anywhere in this hierarchy. No point walking up.
            debug!(
                "cgroup.procs not supported (type: '{}')",
                cgroup_type(&main_cgroup)
            );
            Attempt::GiveUp
        } else {
            debug!(path=%main_cgroup.display(), "failed to enter main cgroup: {e}");
            Attempt::TryParent
        }
    } else if probe_migration(&base) {
        enable_controllers(&base);
        debug!(path=%base.display(), "selected cgroup base");
        return Attempt::Usable(base);
    } else {
        // We are now inside base/main, which is about to be removed: step back
        // out to the ancestor first.
        let _ = std::fs::write(ancestor.join("cgroup.procs"), pid);
        debug!(path=%base.display(), "cgroup.procs probe failed; trying parent");
        Attempt::TryParent
    };

    // Single teardown for every failure above; the removals are no-ops for the
    // directories that were never created.
    let _ = std::fs::remove_dir(&main_cgroup);
    let _ = std::fs::remove_dir(&base);
    attempt
}

/// Fork a child that inherits `base/main` (a leaf) and check that it can
/// migrate to a sibling cgroup by writing to its `cgroup.procs` — exactly what
/// every test child has to do.
fn probe_migration(base: &Path) -> bool {
    let probe = base.join("_probe");
    let _ = std::fs::remove_dir(&probe); // clean up from a crashed prior run
    if std::fs::create_dir(&probe).is_err() {
        return false;
    }
    let ok = probe_cgroup_procs(&probe);
    let _ = std::fs::remove_dir(&probe);
    ok
}

/// Enable the controllers we read stats from for `base`'s children, limited to
/// those its parent already delegated to it (visible in `base/cgroup.controllers`).
/// Never write to the parent's `cgroup.subtree_control` — doing so while the
/// parent has live processes transitions it to "domain invalid" on later runs.
fn enable_controllers(base: &Path) {
    let available = std::fs::read_to_string(base.join("cgroup.controllers")).unwrap_or_default();
    for ctrl in available
        .split_whitespace()
        .filter(|c| matches!(*c, "cpu" | "memory" | "io" | "pids"))
    {
        let _ = std::fs::write(base.join("cgroup.subtree_control"), format!("+{ctrl}"));
    }
}

fn init_base() -> Option<PathBuf> {
    let cg_content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cg_content
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?
        .trim();
    let leaf = PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    let pid = std::process::id().to_string();

    for ancestor in cgroup_ancestors(leaf) {
        match try_ancestor(&ancestor, &pid) {
            Attempt::Usable(base) => return Some(base),
            Attempt::TryParent => continue,
            Attempt::GiveUp => break,
        }
    }

    debug!("no suitable cgroup found in hierarchy");
    None
}

/// Write the calling process's pid into `procs` (a `cgroup.procs` file).
/// Async-signal-safe with no allocation, so it is usable between `fork` and
/// `exec` even though other threads (e.g. the status-bar ticker) exist in the
/// parent. Returns whether the write succeeded.
pub(crate) unsafe fn add_self_to(procs: &CStr) -> bool {
    unsafe {
        let mut buf = [0u8; 12];
        let mut n = buf.len();
        let mut v = libc::getpid() as u64;
        loop {
            n -= 1;
            buf[n] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return false;
        }
        let len = buf.len() - n;
        let written = libc::write(fd, buf[n..].as_ptr().cast(), len);
        libc::close(fd);
        written == len as isize
    }
}

/// Fork a child that writes its own PID to `dir/cgroup.procs` and exits 0 on
/// success or 1 on failure. Returns true if the child exited with 0. The
/// forked child uses only the async-signal-safe [`add_self_to`].
fn probe_cgroup_procs(dir: &Path) -> bool {
    let Ok(procs) = CString::new(dir.join("cgroup.procs").into_os_string().into_vec()) else {
        return false;
    };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        let ok = unsafe { add_self_to(&procs) };
        unsafe { libc::_exit(i32::from(!ok)) }
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

fn read_single_u64(path: impl AsRef<std::path::Path>) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Parse `user_usec` and `system_usec` from `cpu.stat` in a single read.
/// Each field is `None` when the file is absent or the field is missing.
fn read_cpu_usec(path: impl AsRef<std::path::Path>) -> (Option<u64>, Option<u64>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let mut user = None;
    let mut system = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "user_usec" => user = value.trim().parse().ok(),
            "system_usec" => system = value.trim().parse().ok(),
            _ => {}
        }
    }
    (user, system)
}

/// Sum a named field (e.g. `rbytes`) across all device lines in `io.stat`.
/// Returns `None` when the file is absent or the total is zero.
fn read_io_field(cgroup_path: &Path, field: &str) -> Option<u64> {
    let content = std::fs::read_to_string(cgroup_path.join("io.stat")).ok()?;
    let prefix = format!("{field}=");
    let total: u64 = content
        .lines()
        .flat_map(|line| line.split_whitespace())
        .filter_map(|tok| tok.strip_prefix(prefix.as_str()))
        .filter_map(|v| v.parse::<u64>().ok())
        .sum();
    if total > 0 { Some(total) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestors_walk_up_to_but_not_into_the_cgroup_root() {
        let leaf = PathBuf::from("/sys/fs/cgroup/user.slice/user-1000.slice/session-3.scope");
        let walk: Vec<PathBuf> = cgroup_ancestors(leaf.clone()).collect();
        assert_eq!(
            walk,
            vec![
                leaf,
                PathBuf::from("/sys/fs/cgroup/user.slice/user-1000.slice"),
                PathBuf::from("/sys/fs/cgroup/user.slice"),
            ],
            "the walk must stop before /sys/fs/cgroup itself"
        );
    }

    #[test]
    fn ancestors_of_the_cgroup_root_are_just_itself() {
        // A process in the root cgroup (`0::/`) still gets one attempt, but
        // there is nowhere above it to fall back to.
        assert_eq!(
            cgroup_ancestors(PathBuf::from("/sys/fs/cgroup")).collect::<Vec<_>>(),
            vec![PathBuf::from("/sys/fs/cgroup")]
        );
    }

    #[test]
    fn read_cpu_usec_picks_out_both_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stat = tmp.path().join("cpu.stat");
        std::fs::write(&stat, "usage_usec 300\nuser_usec 120\nsystem_usec 180\n").unwrap();
        assert_eq!(read_cpu_usec(&stat), (Some(120), Some(180)));

        // A missing field stays `None` rather than defaulting to zero.
        std::fs::write(&stat, "usage_usec 300\nuser_usec 120\n").unwrap();
        assert_eq!(read_cpu_usec(&stat), (Some(120), None));
        assert_eq!(read_cpu_usec(tmp.path().join("absent")), (None, None));
    }

    #[test]
    fn read_io_field_sums_every_device() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("io.stat"),
            "8:0 rbytes=100 wbytes=7 rios=2 wios=1\n8:16 rbytes=23 wbytes=0\n",
        )
        .unwrap();
        assert_eq!(read_io_field(tmp.path(), "rbytes"), Some(123));
        assert_eq!(read_io_field(tmp.path(), "wbytes"), Some(7));
        // A field no device reports sums to zero, which means "unavailable"
        // rather than a real zero.
        assert_eq!(read_io_field(tmp.path(), "dbytes"), None);
    }
}

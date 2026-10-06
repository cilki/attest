use anyhow::{Result, anyhow};
use brush_parser::ast::{FunctionDefinition, SourceLocation};
use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

use crate::output;
use crate::overlay;

/// Set by the SIGINT/SIGTERM handler. Tests run in their own sessions (see the
/// `setsid` hook in `spawn_test`), so the terminal no longer delivers ^C to
/// them directly; the poll loop watches this flag and tears the run down.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn mark_interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// The test whose xtrace.log is currently being tailed to stderr.
struct Tailed {
    /// Name of the test holding the xtrace output lock.
    name: String,
    file: std::fs::File,
    /// How many bytes have been printed so far.
    offset: u64,
}

/// Live xtrace streaming: at most one test at a time has its xtrace.log tailed
/// to stderr, so concurrent traces never interleave.
struct XtraceStreamer {
    /// `None` while no test holds the xtrace output lock.
    tailed: Option<Tailed>,
}

impl XtraceStreamer {
    fn new() -> Self {
        Self { tailed: None }
    }

    fn is_idle(&self) -> bool {
        self.tailed.is_none()
    }

    /// Start tailing a test's xtrace.log if no other test holds the lock.
    fn try_acquire(&mut self, pending: &PendingTest) {
        if self.tailed.is_some() {
            return;
        }
        let xtrace_path = pending.context.as_ref().unwrap().join("xtrace.log");
        if let Ok(file) = std::fs::File::open(&xtrace_path) {
            eprintln!("\x1b[2m--- xtrace: {} ---\x1b[0m", pending.name);
            self.tailed = Some(Tailed {
                name: pending.name.clone(),
                file,
                offset: 0,
            });
        }
    }

    /// Check whether the tailed xtrace.log has grown since the last flush.
    fn has_new(&self) -> bool {
        self.tailed
            .as_ref()
            .is_some_and(|t| t.file.metadata().is_ok_and(|m| m.len() > t.offset))
    }

    /// Print any bytes appended to the tailed xtrace.log since the last flush.
    fn flush_new(&mut self) {
        let Some(t) = self.tailed.as_mut() else {
            return;
        };
        if t.file.seek(SeekFrom::Start(t.offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if t.file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
            t.offset += buf.len() as u64;
            let _ = write!(std::io::stderr(), "\x1b[2m");
            let _ = std::io::stderr().write_all(&buf);
            let _ = write!(std::io::stderr(), "\x1b[0m");
        }
    }

    /// Settle a finished test's trace output: flush and release the lock if it
    /// was the one being tailed, otherwise dump its whole log (it finished
    /// before the parent could open the file).
    fn finish(&mut self, pending: &PendingTest) {
        if self.tailed.as_ref().is_some_and(|t| t.name == pending.name) {
            self.flush_new();
            self.tailed = None;
        } else {
            dump_xtrace_log(&pending.name, pending.context.as_ref().unwrap());
        }
    }
}

/// Print a test's full xtrace.log, dimmed, under a `--- xtrace: <name> ---`
/// header. Silent if the log is missing or empty.
fn dump_xtrace_log(name: &str, context: &Path) {
    if let Ok(content) = std::fs::read(context.join("xtrace.log"))
        && !content.is_empty()
    {
        eprintln!("\x1b[2m--- xtrace: {name} ---\x1b[0m");
        let _ = write!(std::io::stderr(), "\x1b[2m");
        let _ = std::io::stderr().write_all(&content);
        let _ = write!(std::io::stderr(), "\x1b[0m");
    }
}

pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub timed_out: bool,
    pub duration: Duration,
    pub context: PathBuf,
    pub source_path: PathBuf,
    #[cfg(feature = "cgroup")]
    pub resources: Option<crate::cgroup::ResourceStats>,
}

/// State held by the parent for a spawned test child that has not yet been
/// waited on. Dropping this kills the child (if still running) and cleans up.
struct PendingTest {
    child: Child,
    /// Set once the child has been waited on. The kernel is free to hand its
    /// pid to a new process from that moment, so nothing may be signalled
    /// through it any more — see [`PendingTest::kill_tree`].
    reaped: bool,
    /// Set to `true` when the child was killed due to exceeding `--timeout`.
    timed_out: bool,
    name: String,
    start: Instant,
    /// `None` after the path has been transferred to `TestResult`.
    context: Option<PathBuf>,
    source_path: PathBuf,
    #[cfg(feature = "cgroup")]
    cgroup: Option<crate::cgroup::TestCgroup>,
}

impl PendingTest {
    /// Kill the test's entire process tree: the child was made a session (and
    /// process-group) leader at spawn, so its pgid is its pid. The cgroup, when
    /// present, also catches processes that re-`setsid`'d themselves.
    ///
    /// Both of those are only meaningful while the child is unreaped: once it
    /// has been waited on the kernel may hand its pid — and therefore this
    /// pgid — to an unrelated process, and `kill(-pgid)` would SIGKILL
    /// somebody else's process group. So pid-based signalling stops at that
    /// point and only `cgroup.kill`, which is keyed to a directory and can
    /// never be misdirected, still runs. (`Child::kill` is no help as a guard:
    /// it quietly succeeds on an already-waited-for child.)
    fn kill_tree(&mut self) {
        #[cfg(feature = "cgroup")]
        if let Some(ref cg) = self.cgroup {
            cg.kill_all();
        }
        if self.reaped {
            return;
        }
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.kill();
    }

    /// Has the child exited? `waitid` with `WNOWAIT` answers that without
    /// consuming the zombie, so [`Child::try_wait`] can still reap it and the
    /// pid — and the pgid derived from it — stays reserved for as long as
    /// [`Self::reap`] needs to signal the group.
    fn has_exited(&self) -> std::io::Result<bool> {
        // Zeroed up front because `waitid` leaves the struct untouched when
        // `WNOHANG` finds nothing, and `si_pid == 0` is how that is reported.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if ret == -1 {
            let err = std::io::Error::last_os_error();
            // A signal cut the call short; the next poll pass asks again.
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err);
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    /// Reap the child if it has exited, killing its process tree first.
    /// `None` while it is still running.
    ///
    /// The order matters: the test's pgid is the child's pid, and that number
    /// stays reserved only until the child is reaped. Killing the group first
    /// and consuming the zombie second is what makes `kill(-pgid)` provably
    /// land on this test and nothing else.
    fn reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if !self.reaped {
            if !self.has_exited()? {
                return Ok(None);
            }
            // Background processes the test left behind outlive the shell;
            // tests are promised they never have to clean those up themselves.
            self.kill_tree();
            self.reaped = true;
        }
        self.child.try_wait()
    }
}

impl Drop for PendingTest {
    fn drop(&mut self) {
        // Kill the tree in case the test was abandoned before being reaped
        // (--bail, a timeout, ^C). A test that went through `reap` was already
        // torn down there, and `kill_tree` is a no-op once that has happened.
        self.kill_tree();
        let _ = self.child.wait();
        if let Some(ref dir) = self.context {
            let _ = std::fs::remove_dir_all(dir);
        }
        // cgroup field drops here, removing the cgroup directory
    }
}

/// A `--override` spec: a binary to copy into the test context's `bin/` dir.
///
/// Accepted CLI forms:
/// - `/usr/bin/example` (absolute path) — copied as `bin/example`
/// - `./bin/example` (relative path) — copied as `bin/example`
/// - `example=/usr/bin/override` — copies `/usr/bin/override` to `bin/example`
#[derive(Clone, Debug)]
pub struct OverrideSpec {
    pub name: String,
    pub source: PathBuf,
}

impl std::str::FromStr for OverrideSpec {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if let Some((name, path)) = s.split_once('=') {
            if name.is_empty() || path.is_empty() {
                return Err(format!("invalid override mapping: {s}"));
            }
            if name.contains('/') {
                return Err(format!("override name must not contain '/': {name}"));
            }
            return Ok(OverrideSpec {
                name: name.to_string(),
                source: PathBuf::from(path),
            });
        }

        let path = PathBuf::from(s);
        if !s.contains('/') {
            return Err(format!(
                "override `{s}` must be a path (e.g. `/usr/bin/{s}`) or a mapping (e.g. `{s}=/path/to/bin`)"
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| format!("invalid override path: {s}"))?
            .to_string_lossy()
            .into_owned();
        Ok(OverrideSpec { name, source: path })
    }
}

#[derive(Default)]
pub struct RunConfig {
    pub parallel: usize,
    pub bail: bool,
    /// Output verbosity: 0 prints only failures, 1 adds per-test PASS/FAIL
    /// lines, 2+ adds live xtrace streaming.
    pub verbose: u8,
    pub json: bool,
    /// When set, each test's logs, filesystem delta and working directory are
    /// copied here once it finishes (see [`save_test_context`]). Context dirs
    /// themselves always live in the run's tempdir and are cleaned up on exit.
    pub save_context: Option<PathBuf>,
    pub override_cmds: Vec<OverrideSpec>,
    /// Directories prepended to each test's PATH (e.g. build-cache output dirs).
    /// Lower precedence than `override_cmds`/context `bin/`, higher than inherited PATH.
    pub bin_dirs: Vec<PathBuf>,
    pub strace: Vec<String>,
    /// Wall-clock timeout per test. Tests exceeding this are killed and marked as timed out.
    pub timeout: Option<Duration>,
    /// Randomly SIGSTOP/SIGCONT individual descendant processes of each test to introduce
    /// timing non-determinism.
    pub fuzz: Option<f64>,
    /// Override the shell used to run test scripts, ignoring the script's own shebang.
    pub shebang: Option<String>,
    /// Disable overlayfs isolation; run each test directly in the working directory.
    pub no_overlay: bool,
    #[cfg(feature = "cgroup")]
    pub no_cgroups: bool,
}

/// Run-wide isolation state shared by every spawned test.
struct RunEnv {
    /// Directory attest was invoked from; each test starts here (inside its
    /// ephemeral root when isolation is active).
    invocation_dir: PathBuf,
    overlay_mode: Option<overlay::Mode>,
    /// How each mount under `/` is re-established inside per-test roots.
    submounts: Vec<overlay::Submount>,
}

/// One test to run: its unique display name, the shell function to invoke, all
/// functions extracted from its file (the test plus any helpers), and the path
/// of that file.
pub type TestSpec<'a> = (&'a str, &'a str, &'a [FunctionDefinition], &'a Path);

pub fn run_all_tests(tests: Vec<TestSpec<'_>>, config: &RunConfig) -> Result<Vec<TestResult>> {
    let mut results = Vec::new();
    let total = tests.len();
    let mut status = output::StatusDisplay::new(total, config.json);
    let wall_start = Instant::now();

    let max_parallel = config.parallel.max(1);
    let mut test_iter = tests.into_iter();
    let mut pending_list: Vec<PendingTest> = Vec::new();
    let mut bail_flag = false;
    let mut rng: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xdeadbeef_cafebabe);
    let mut xtrace = if config.verbose >= 2 {
        Some(XtraceStreamer::new())
    } else {
        None
    };

    let tmp = tempfile::TempDir::new()?;

    // Tests run in their own sessions (setsid in spawn_test), so the terminal
    // no longer delivers ^C to them; catch it here and tear the run down.
    unsafe {
        let handler = mark_interrupted as extern "C" fn(libc::c_int) as usize;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    // Each test runs in a whole-root overlay: `/` is the read-only lower layer
    // and writes land in per-test upper layers. The submount plan (which mounts
    // get their own ephemeral overlay, which are rebound live) is computed once
    // and shared across tests; the probe rehearses the full setup with it.
    let invocation_dir = std::env::current_dir()?;
    let submounts = overlay::compute_submounts(&invocation_dir);
    let overlay_mode = if config.no_overlay {
        None
    } else {
        overlay::probe_support(tmp.path(), &invocation_dir, &submounts)
    };
    match overlay_mode {
        None if config.no_overlay => debug!("overlay isolation disabled via --no-overlay"),
        None => warn!("overlayfs unavailable; tests run without filesystem isolation"),
        Some(overlay::Mode::Userns) => {
            debug!("using unprivileged overlay; tests run as root inside their namespace")
        }
        Some(overlay::Mode::Privileged) => debug!("using privileged overlay isolation"),
    }
    let env = RunEnv {
        invocation_dir,
        overlay_mode,
        submounts,
    };

    // Contexts always live in the temp dir; `--save-context` copies each test's
    // upper layer and logs out afterward (see save_test_context).
    let contexts_dir = tmp.path();
    if let Some(ref save_dir) = config.save_context {
        std::fs::create_dir_all(save_dir)?;
    }

    let mut context_names = std::collections::HashSet::new();
    let mut spawn = |(display_name, fn_name, all_functions, source_path): TestSpec| {
        let dir = context_dir_name(display_name, &mut context_names);
        spawn_test(
            display_name,
            fn_name,
            all_functions,
            source_path,
            contexts_dir.join(dir),
            config,
            &env,
        )
    };

    // Seed the initial batch up to max_parallel.
    while pending_list.len() < max_parallel {
        let Some(test) = test_iter.next() else { break };
        pending_list.push(spawn(test)?);
    }

    // Poll loop: non-blocking reap, process completions, update status.
    while !pending_list.is_empty() {
        // A SIGINT/SIGTERM arrived: kill every test tree and abort the run.
        if INTERRUPTED.load(Ordering::Relaxed) {
            let n = pending_list.len();
            pending_list.clear(); // drop kills the trees and removes contexts
            status.finish();
            anyhow::bail!("interrupted; killed {n} running test(s)");
        }

        // Stream xtrace output from the current holder.
        if let Some(ref mut xt) = xtrace
            && xt.has_new()
        {
            status.suspend(|| xt.flush_new());
        }

        // Kill any tests that have exceeded the wall-clock timeout.
        if let Some(timeout) = config.timeout {
            for pending in pending_list.iter_mut() {
                if !pending.timed_out && pending.start.elapsed() > timeout {
                    pending.kill_tree();
                    pending.timed_out = true;
                }
            }
        }

        // Randomly pause/resume individual descendant processes to introduce
        // timing fuzziness.
        if let Some(fuzz_level) = config.fuzz {
            for pending in &pending_list {
                fuzz_tick(pending.child.id(), fuzz_level, &mut rng);
            }
        }

        // Non-blocking reap: walk the pending list, taking out whatever has
        // finished. `i` only advances past tests that are still running, so a
        // removal leaves it pointing at the next candidate.
        let mut completed: Vec<TestResult> = Vec::new();
        let mut i = 0;
        while i < pending_list.len() {
            let exit_status = match pending_list[i].reap() {
                Ok(Some(status)) => status,
                Ok(None) => {
                    i += 1; // still running
                    continue;
                }
                Err(e) => return Err(anyhow!("reaping test child failed: {e}")),
            };
            if let Some(ref mut xt) = xtrace {
                status.suspend(|| xt.finish(&pending_list[i]));
            }
            let pending = pending_list.remove(i);
            if bail_flag {
                continue; // Drop kills + cleans up
            }
            let result = build_result(pending, exit_status);
            if let Some(ref save_dir) = config.save_context {
                save_test_context(&result, save_dir, &env.submounts);
            }
            completed.push(result);
        }

        // Print results and start new tests.
        // Sort completed by name for deterministic output within a reap batch.
        completed.sort_by(|a, b| a.name.cmp(&b.name));
        for result in completed {
            if config.json {
                output::print_test_result_json(&result);
            } else {
                status.record(result.passed);
                if config.verbose >= 1 || !result.passed {
                    status.suspend(|| output::print_test_result(&result));
                }
                if !result.passed {
                    // At -vv the streamer already showed this test's xtrace.
                    if config.verbose < 2 {
                        status.suspend(|| dump_xtrace_log(&result.name, &result.context));
                    }
                    status.suspend(|| crate::diagnostics::print_failure_snippet(&result));
                }
            }
            if !result.passed && config.bail {
                bail_flag = true;
            }
            results.push(result);

            if !bail_flag && let Some(test) = test_iter.next() {
                pending_list.push(spawn(test)?);
            }
        }

        if pending_list.is_empty() {
            break;
        }

        // If xtrace lock is free, acquire the next pending test.
        if let Some(ref mut xt) = xtrace
            && xt.is_idle()
            && let Some(p) = pending_list.first()
        {
            status.suspend(|| xt.try_acquire(p));
        }

        // Update status line with currently running tests.
        let running: Vec<(&str, Duration)> = pending_list
            .iter()
            .map(|p| {
                #[cfg(feature = "cgroup")]
                let duration = p
                    .cgroup
                    .as_ref()
                    .and_then(|cg| cg.read_cpu_time())
                    .unwrap_or_else(|| p.start.elapsed());
                #[cfg(not(feature = "cgroup"))]
                let duration = p.start.elapsed();
                (p.name.as_str(), duration)
            })
            .collect();
        status.update(&running, results.len());

        std::thread::sleep(Duration::from_millis(50));
    }

    status.finish();

    if !config.json {
        output::print_summary(&results, wall_start.elapsed());
    }

    Ok(results)
}

/// Derive the name of a test's context directory from its display name,
/// keeping it a single path component that is unique within the run.
///
/// Test names come from shell function definitions and shells accept almost
/// anything there, `/` and `..` included (`test_x/../../../etc() { … }` parses
/// and runs fine under sh and bash). Joining such a name onto the run's temp
/// dir escapes it, and the escape is not contained by test isolation: the
/// parent process creates the context dir, writes the generated script and
/// logs into it, and `remove_dir_all`s it on drop, all outside any overlay and
/// with the invoking user's privileges. `--save-context` would likewise copy
/// the test's files outside the directory that was asked for.
///
/// So the display name is kept verbatim for output and the directory name is
/// derived from it, the same way the per-test cgroup name is (see
/// [`crate::cgroup::TestCgroup::try_create`]). Only separators are replaced —
/// everything else (`:` for file-qualified names, `#` for `--repeat`) is
/// harmless in a path component and worth keeping legible. Distinct names that
/// collapse to the same directory name get a numeric suffix, since contexts
/// must never be shared between tests.
fn context_dir_name(display_name: &str, taken: &mut std::collections::HashSet<String>) -> String {
    let mut base: String = display_name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    if base.is_empty() || base == "." || base == ".." {
        base = format!("_{base}");
    }
    if taken.insert(base.clone()) {
        return base;
    }
    let mut k = 2;
    loop {
        let candidate = format!("{base}_{k}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        k += 1;
    }
}

/// Whether `shell` names something we can exec: an existing path when it
/// contains a `/`, or a command resolvable on `PATH` otherwise.
pub(crate) fn shell_exists(shell: &str) -> bool {
    if shell.contains('/') {
        std::path::Path::new(shell).exists()
    } else {
        which::which(shell).is_ok()
    }
}

/// Resolve a shell name or path to an executable, falling back to `/bin/sh`
/// when the requested shell is not found.
fn resolve_shell(shell: &str) -> String {
    if shell_exists(shell) {
        shell.to_string()
    } else {
        "/bin/sh".to_string()
    }
}

/// Build the script that the runner sources to define a test's functions.
///
/// Each function is emitted verbatim at the same line number it occupies in the
/// original source, with the gaps between them blanked out. Because bash's
/// `$LINENO` (used by our xtrace `PS4`) reports the line within the sourced
/// file, keeping functions at their original offsets makes xtrace line numbers
/// match the source exactly — so a failing command's trace points straight at
/// its source line, with no reconstruction or line-mapping heuristics needed on
/// the diagnostics side.
///
/// Only function definitions are emitted (top-level code is dropped), matching
/// the previous `to_string`-based behavior, so sourcing the script never runs a
/// test's top-level statements.
///
/// `source` is `None` when the original file could not be read. Then — as for
/// any single function the parser gave no location for — the function falls
/// back to the reformatted AST rendering: line alignment is lost, but the
/// script still defines and runs the same functions.
fn build_functions_source(functions: &[FunctionDefinition], source: Option<&str>) -> String {
    let src_lines: Vec<&str> = source.unwrap_or_default().lines().collect();
    let mut out = String::new();
    // 1-based line number of the next line to be written to `out`.
    let mut line = 1usize;

    for func in functions {
        let located = if src_lines.is_empty() {
            None
        } else {
            func.location()
        };
        let Some(span) = located else {
            for l in func.to_string().lines() {
                out.push_str(l);
                out.push('\n');
                line += 1;
            }
            continue;
        };

        // Functions are yielded in source order, so this only pads forward.
        while line < span.start.line {
            out.push('\n');
            line += 1;
        }

        // `end.line` is the line of the closing brace (inclusive here).
        let last = span.end.line.min(src_lines.len());
        for idx in span.start.line..=last {
            if let Some(text) = src_lines.get(idx - 1) {
                out.push_str(text);
            }
            out.push('\n');
            line += 1;
        }
    }

    out
}

/// Spawn a child process that will run the test. Returns a `PendingTest` that
/// the caller must reap (or simply drop to kill+clean up). `context` must be
/// unique per test (the caller derives it from the unique display name).
fn spawn_test(
    display_name: &str,
    fn_name: &str,
    all_functions: &[FunctionDefinition],
    source_path: &Path,
    context: PathBuf,
    config: &RunConfig,
    env: &RunEnv,
) -> Result<PendingTest> {
    std::fs::create_dir_all(&context)?;

    // Fresh, empty working directory for the test. It lives in the context dir
    // (live-bound inside isolated roots), so it works with and without overlay
    // isolation and is picked up by --save-context.
    std::fs::create_dir_all(cwd_dir(&context))?;

    // When overlay isolation is available, plan the per-test ephemeral root
    // (this also creates the overlay dirs under the context dir).
    let root_plan = match env.overlay_mode {
        Some(mode) => Some(overlay::RootOverlay::build(
            mode,
            &context,
            &env.invocation_dir,
            &env.submounts,
        )?),
        None => None,
    };

    let script_path = context.join("functions.sh");
    // Read the original source so functions can be emitted verbatim at their
    // original line numbers (see build_functions_source).
    let source = std::fs::read_to_string(source_path).ok();
    let script = build_functions_source(all_functions, source.as_deref());
    std::fs::write(&script_path, &script)?;

    if !config.override_cmds.is_empty() {
        let bin: &Path = &context.join("bin");
        std::fs::create_dir_all(bin)?;

        for spec in &config.override_cmds {
            let src = &spec.source;
            if !src.exists() {
                return Err(anyhow!(
                    "--override: source path does not exist: {}",
                    src.display()
                ));
            }
            let dst = bin.join(&spec.name);

            debug!(src=%src.display(), dst=%dst.display(), "Overriding command");
            std::fs::copy(src, &dst)?;
        }
    }

    if !config.strace.is_empty() {
        create_strace_wrappers(&context, &config.strace)?;
    }

    let source_path_owned = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    let shell = if let Some(s) = config.shebang.as_deref() {
        resolve_shell(s)
    } else {
        resolve_shell(&crate::discovery::get_script_shell(&source_path_owned))
    };

    #[cfg(feature = "cgroup")]
    let cgroup = if config.no_cgroups {
        None
    } else {
        crate::cgroup::TestCgroup::try_create(display_name)
    };

    let runner_content = build_runner_script(
        fn_name,
        &script_path,
        &context,
        &config.bin_dirs,
        &config.strace,
    );
    // <shell> -c <script> <source_path>: passing source_path as argv[0]
    // makes $0 inside the test functions refer to the original script.
    let source_str = source_path_owned.to_str().unwrap_or("bash").to_string();
    let mut cmd = Command::new(&shell);
    cmd.args(["-c", &runner_content, &source_str]);

    // Detach into a fresh session/process group so timeouts and cleanup can
    // kill the whole test tree with one kill(-pgid).
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    // Place the child into its cgroup after fork, before exec. The parent has
    // live threads (status-bar ticker), so the hook must not allocate:
    // add_self_to is async-signal-safe and the path CString is built here.
    // TODO don't include test setup in cgroup
    #[cfg(feature = "cgroup")]
    if let Some(ref cg) = cgroup
        && let Some(procs) = cg.procs_cstring()
    {
        unsafe {
            cmd.pre_exec(move || {
                crate::cgroup::add_self_to(&procs);
                Ok(())
            });
        }
    }

    // Registered after the cgroup hook so cgroup placement happens in the host
    // namespace before we unshare into a private mount namespace.
    let isolated = root_plan.is_some();
    if let Some(plan) = root_plan {
        overlay::register_root_mount(&mut cmd, plan);
    }

    let start = Instant::now();
    let child = cmd.spawn().map_err(|e| {
        if isolated {
            anyhow!(
                "spawn failed for {display_name}: {e} \
                 (isolation was active for this test; --no-overlay disables it)"
            )
        } else {
            anyhow!("spawn failed for {display_name}: {e}")
        }
    })?;

    Ok(PendingTest {
        child,
        reaped: false,
        timed_out: false,
        name: display_name.to_string(),
        start,
        context: Some(context),
        source_path: source_path_owned,
        #[cfg(feature = "cgroup")]
        cgroup,
    })
}

/// Build a `TestResult` from a `PendingTest` whose child has already exited
/// with the given status.
fn build_result(mut pending: PendingTest, status: ExitStatus) -> TestResult {
    let duration = pending.start.elapsed();
    let timed_out = pending.timed_out;
    let passed = !timed_out && status.success();

    // Read stats before dropping cgroup (which removes the directory).
    #[cfg(feature = "cgroup")]
    let resources = pending.cgroup.as_ref().map(|cg| cg.read_stats());

    TestResult {
        name: pending.name.clone(),
        passed,
        timed_out,
        duration,
        context: pending.context.take().unwrap(),
        source_path: pending.source_path.clone(),
        #[cfg(feature = "cgroup")]
        resources,
    }
    // pending drops here: reaped=true skips kill/wait, tmp_dir=None skips
    // dir removal, cgroup drops removing the cgroup directory.
}

/// Does `dir` exist and contain at least one entry?
fn dir_non_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// Copy the files a finished test created or modified (the upper layers of its
/// root overlay and of each ephemeral submount overlay, merged and laid out by
/// absolute path — a write to `/tmp/x` lands at `<dst>/tmp/x`) plus its logs
/// into `<save_dir>/<context dir name>` for `--save-context`. The temp context dir is
/// otherwise discarded when the run's tempdir is dropped.
///
/// A delta is whatever the test chose to write, symlinks included, so every
/// write made here stays inside `save_dir` by construction: names reproduced
/// from the delta are never followed as symlinks when something else has to be
/// written at (or under) them.
fn save_test_context(result: &TestResult, save_dir: &Path, submounts: &[overlay::Submount]) {
    // Named after the context dir rather than the test: that name is already a
    // single safe component (see context_dir_name), so a test whose function
    // name contains `/` cannot steer the copy outside save_dir.
    let Some(dst) = result.context.file_name().map(|n| save_dir.join(n)) else {
        warn!(
            "failed to save context for {}: unnamed context dir",
            result.name
        );
        return;
    };
    let upper = overlay::upper_dir(&result.context);
    if upper.is_dir() {
        if let Err(e) = overlay::copy_dir_recursive(&upper, &dst) {
            warn!("failed to save context for {}: {e}", result.name);
        }
    } else if let Err(e) = std::fs::create_dir_all(&dst) {
        warn!("failed to save context for {}: {e}", result.name);
        return;
    }
    for (i, sm) in submounts.iter().filter(|s| s.ephemeral).enumerate() {
        let sub_upper = overlay::submount_upper_dir(&result.context, i);
        if !dir_non_empty(&sub_upper) {
            continue;
        }
        let rel = sm.source.strip_prefix("/").unwrap_or(&sm.source);
        // `rel` is laid out inside the tree just filled from the root upper
        // layer, so any of its leading components may be a symlink the test
        // planted (e.g. `/var` replaced by a link to the user's home): create
        // them explicitly instead of letting `create_dir_all` follow one.
        let saved = overlay::create_dir_nofollow(&dst, rel)
            .and_then(|sub_dst| overlay::copy_dir_recursive(&sub_upper, &sub_dst));
        if let Err(e) = saved {
            warn!(
                "failed to save {} delta for {}: {e}",
                sm.source.display(),
                result.name
            );
        }
    }
    // The per-test working directory is live-bound inside isolated roots (it
    // is not part of any overlay upper layer), so copy it explicitly.
    let cwd = cwd_dir(&result.context);
    if dir_non_empty(&cwd)
        && let Err(e) = overlay::copy_dir_recursive(&cwd, &dst.join("cwd"))
    {
        warn!("failed to save cwd for {}: {e}", result.name);
    }
    // `--strace` logs are written straight into the context dir (not through
    // any overlay), so they need copying too — otherwise they die with the
    // temp context and there is no way to read them.
    let strace = result.context.join("strace");
    if dir_non_empty(&strace)
        && let Err(e) = overlay::copy_dir_recursive(&strace, &dst.join("strace"))
    {
        warn!("failed to save strace logs for {}: {e}", result.name);
    }
    for log in ["stdout.log", "xtrace.log"] {
        let src = result.context.join(log);
        if src.exists() {
            // A test that created `/stdout.log` as a symlink has had it copied
            // here verbatim; writing the real log through it would land on
            // whatever host path it names.
            let to = dst.join(log);
            let _ = overlay::unlink_if_symlink(&to);
            let _ = std::fs::copy(&src, to);
        }
    }
}

/// The clean per-test working directory inside a context dir.
pub fn cwd_dir(context: &Path) -> PathBuf {
    context.join("cwd")
}

/// Quote a path for literal inclusion in generated sh scripts: single-quoted,
/// with embedded single quotes escaped, so spaces and metacharacters survive.
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// Build the shell script content that sources the function definitions and
/// runs the named test. Used with `/bin/sh -c <content> <source_path>` so
/// that `$0` inside test functions refers to the original script.
fn build_runner_script(
    test_name: &str,
    functions_path: &Path,
    working_dir: &Path,
    bin_dirs: &[PathBuf],
    strace: &[String],
) -> String {
    use std::fmt::Write;

    let mut s = String::new();

    // PATH entries prepended low-to-high precedence, each layering on top of the
    // previous, so the last one wins:
    //   --bin-dir dirs  — below the context bin/ (--override wins) but above the inherited PATH
    //   context bin/    — --override binaries take precedence over --bin-dir
    //   strace_bin/     — wrappers must precede bin/ so they intercept calls
    let mut path_dirs: Vec<PathBuf> = bin_dirs.to_vec();
    path_dirs.push(working_dir.join("bin"));
    if !strace.is_empty() {
        path_dirs.push(working_dir.join("strace_bin"));
    }
    for dir in &path_dirs {
        let _ = writeln!(s, "export PATH={}:\"$PATH\"", sh_quote(dir));
    }

    // Redirect both stdout and stderr to log files, then enable xtrace.
    let stdout = working_dir.join("stdout.log");
    let xtrace = working_dir.join("xtrace.log");
    let _ = writeln!(s, "exec 1>{} 2>{}", sh_quote(&stdout), sh_quote(&xtrace));
    s.push_str("set -e\n");

    // Every test starts in its own clean, empty working directory.
    let _ = writeln!(s, "cd {}", sh_quote(&cwd_dir(working_dir)));

    // Source function definitions, then enable xtrace and invoke the test function.
    let _ = writeln!(s, ". {}", sh_quote(functions_path));
    s.push_str("PS4='+$LINENO: '\n");
    s.push_str("set -x\n");
    s.push_str(test_name);
    s.push('\n');

    s
}

/// Advance an xorshift64 PRNG state in place and return the new value. Used by
/// the `--fuzz` scheduler to pick which descendant to pause or resume.
fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// One `--fuzz` tick for the test rooted at `test_pid`: either pause a random
/// running descendant or resume a random stopped one, never both. `level` is
/// the aggressiveness in (0,1) — the higher it is, the more often a tick pauses
/// rather than resumes.
fn fuzz_tick(test_pid: u32, level: f64, rng: &mut u64) {
    let resume = (xorshift64(rng) as f64) / (u64::MAX as f64) >= level;
    let tree = collect_descendants(test_pid);
    // A descendant whose state can't be read counts as running. The pick is
    // only a preference: `signal_tree_member` re-checks the state of whatever
    // it is about to signal, so a process that changed state (or vanished)
    // meanwhile is skipped rather than signalled anyway.
    let candidates: Vec<u32> = tree
        .iter()
        .copied()
        .filter(|&pid| pid != test_pid && (process_state(pid) == Some('T')) == resume)
        .collect();
    if candidates.is_empty() {
        return;
    }
    let chosen = candidates[(xorshift64(rng) as usize) % candidates.len()];
    let (signal, verb) = if resume {
        (libc::SIGCONT, "Resumed")
    } else {
        (libc::SIGSTOP, "Paused")
    };
    if signal_tree_member(chosen, &tree, resume, signal) {
        trace!(pid = chosen, "{verb} subprocess");
    }
}

/// Send `signal` to `pid`, but only if it is still the member of `tree` the
/// caller took it for and its stopped-ness still matches `want_stopped`.
/// Returns whether the signal was delivered.
///
/// `pid` came out of a `/proc` walk a few syscalls ago, and a pid is free to be
/// reissued the moment its process is reaped — so signalling the number blind
/// can land on an unrelated process, including one attest does not own at all.
/// `SIGSTOP` makes that particularly unpleasant: nothing ever undoes it, since
/// later ticks only resume processes they still find inside the test's tree, so
/// a misdirected pause leaves somebody else's process stopped for good (every
/// process on the host, when attest runs as root for privileged isolation).
///
/// Pinning the pid first rules it out: identity, state and signal all travel
/// through one descriptor that the kernel never re-points at the pid's next
/// occupant, and the parent check rejects a number that has already moved on.
fn signal_tree_member(pid: u32, tree: &[u32], want_stopped: bool, signal: libc::c_int) -> bool {
    let Some(target) = PinnedProcess::open(pid) else {
        return false;
    };
    // Read through the pinned descriptor, so the process described here is
    // provably the one signalled below.
    let Some(stat) = target.stat() else {
        return false; // already gone
    };
    if (stat_state(&stat) == Some('T')) != want_stopped {
        return false;
    }
    // Still ours? A reissued pid belongs to a process from outside the test, so
    // its parent is not one of the processes the tree walk just found.
    if !stat_ppid(&stat).is_some_and(|ppid| tree.contains(&ppid)) {
        debug!(pid, "fuzz: pid left the test tree; not signalling it");
        return false;
    }
    target.send_signal(signal)
}

/// A process pinned by an open descriptor on its `/proc/<pid>` directory.
///
/// procfs gives every incarnation of a pid its own directory inode, and Linux
/// accepts such a descriptor as a pidfd, so reads and signals issued through
/// one either reach the process it was opened for or fail with `ESRCH`. That is
/// what makes it safe to act on a pid read out of `/proc`: the number may be
/// recycled in the meantime, but this descriptor does not follow it.
struct PinnedProcess(libc::c_int);

impl PinnedProcess {
    /// Pin `pid`. `None` if it is already gone (or never existed).
    fn open(pid: u32) -> Option<Self> {
        let path = std::ffi::CString::new(format!("/proc/{pid}")).ok()?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        (fd >= 0).then_some(Self(fd))
    }

    /// This process's `stat` line, or `None` once it is gone (reading anything
    /// under the directory of a dead task fails with `ESRCH`).
    fn stat(&self) -> Option<String> {
        use std::os::fd::FromRawFd;

        let fd =
            unsafe { libc::openat(self.0, c"stat".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mut stat = String::new();
        unsafe { std::fs::File::from_raw_fd(fd) }
            .read_to_string(&mut stat)
            .ok()?;
        Some(stat)
    }

    /// Deliver `signal` to the pinned process via `pidfd_send_signal(2)`.
    fn send_signal(&self, signal: libc::c_int) -> bool {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0,
                signal,
                std::ptr::null_mut::<libc::siginfo_t>(),
                0,
            ) == 0
        }
    }
}

impl Drop for PinnedProcess {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Collect the PID of `root` and all of its descendants by walking
/// `/proc/<pid>/task/<pid>/children` recursively.
fn collect_descendants(root: u32) -> Vec<u32> {
    let mut result = vec![root];
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        let path = format!("/proc/{}/task/{}/children", pid, pid);
        if let Ok(s) = std::fs::read_to_string(path) {
            for child in s.split_whitespace().filter_map(|t| t.parse::<u32>().ok()) {
                result.push(child);
                queue.push(child);
            }
        }
    }
    result
}

/// Read the single-character process state from `/proc/<pid>/stat`.
/// Returns `None` if the file cannot be read (e.g. the process has already exited).
fn process_state(pid: u32) -> Option<char> {
    stat_state(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The fields of a `/proc/<pid>/stat` line from `state` (field 3) on. The
/// `comm` field before it is parenthesized and may itself contain spaces and
/// parens, so the *last* `)` is what reliably locates the rest.
fn stat_fields(stat: &str) -> Option<std::str::SplitWhitespace<'_>> {
    Some(stat[stat.rfind(')')? + 1..].split_whitespace())
}

/// The single-character state field of a `/proc/<pid>/stat` line.
fn stat_state(stat: &str) -> Option<char> {
    stat_fields(stat)?.next()?.chars().next()
}

/// The parent pid field of a `/proc/<pid>/stat` line.
fn stat_ppid(stat: &str) -> Option<u32> {
    stat_fields(stat)?.nth(1)?.parse().ok()
}

fn create_strace_wrappers(working_dir: &Path, commands: &[String]) -> Result<()> {
    // Resolve strace here rather than leaving `strace` for the wrapper to look
    // up at run time: a missing strace would otherwise surface as every traced
    // test failing with `exec: strace: not found` in its xtrace, and an
    // absolute path also survives tests that rewrite PATH.
    let strace = which::which("strace").map_err(|_| {
        anyhow!("--strace: `strace` not found on PATH; install it to trace commands")
    })?;

    let strace_bin = working_dir.join("strace_bin");
    std::fs::create_dir_all(&strace_bin)?;

    let strace_dir = working_dir.join("strace");
    std::fs::create_dir_all(&strace_dir)?;

    for cmd in commands {
        let real_path =
            which::which(cmd).map_err(|_| anyhow!("--strace: command not found: {cmd}"))?;

        // The wrapper is only ever reached by name through PATH, so it is named
        // after the command's last component. Joining the spec itself would let
        // `--strace /usr/bin/curl` (or `--strace ../x`) escape the context dir
        // and overwrite the named binary with the wrapper script — fatal when
        // attest runs under sudo, as overlay isolation often requires.
        let name = Path::new(cmd)
            .file_name()
            .ok_or_else(|| anyhow!("--strace: not a command: {cmd}"))?;
        let wrapper = strace_bin.join(name);
        let strace_out = strace_dir.join(format!("{}.log", name.to_string_lossy()));
        let script = format!(
            "#!/bin/sh\nexec {} -f -o {} {} \"$@\"\n",
            sh_quote(&strace),
            sh_quote(&strace_out),
            sh_quote(&real_path),
        );
        std::fs::write(&wrapper, script)?;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A RunEnv with isolation disabled, for direct spawn_test tests.
    fn no_overlay_env(invocation_dir: &Path) -> RunEnv {
        RunEnv {
            invocation_dir: invocation_dir.to_path_buf(),
            overlay_mode: None,
            submounts: Vec::new(),
        }
    }

    /// Write `script` to `dir/t.sh` and parse it.
    fn parse_script(dir: &Path, script: &str) -> (PathBuf, crate::parser::TestFile) {
        let path = dir.join("t.sh");
        fs::write(&path, script).unwrap();
        let tf = crate::parser::parse_test_file(&path).unwrap();
        (path, tf)
    }

    /// Block until `pending` finishes, reaping it exactly the way the poll
    /// loop in `run_all_tests` does so tests exercise that path.
    fn reap_blocking(pending: &mut PendingTest) -> ExitStatus {
        loop {
            if let Some(status) = pending.reap().expect("reap failed") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Spawn `test_name` out of `script` (written under `srcdir`) and block
    /// until it finishes.
    fn run_script(
        srcdir: &Path,
        script: &str,
        test_name: &str,
        config: &RunConfig,
        env: &RunEnv,
    ) -> TestResult {
        let (path, tf) = parse_script(srcdir, script);
        let ctx = TempDir::new().unwrap().keep();
        let mut pending =
            spawn_test(test_name, test_name, &tf.functions, &path, ctx, config, env).unwrap();
        let status = reap_blocking(&mut pending);
        build_result(pending, status)
    }

    /// `run_all_tests` input running every test in `tf` under its own name.
    fn test_refs<'a>(tf: &'a crate::parser::TestFile, path: &'a Path) -> Vec<TestSpec<'a>> {
        tf.tests
            .iter()
            .map(|t| {
                (
                    t.name.as_str(),
                    t.name.as_str(),
                    tf.functions.as_slice(),
                    path,
                )
            })
            .collect()
    }

    /// Run `test_name` from `script` without isolation or extra configuration.
    fn run_inline(script: &str, test_name: &str) -> TestResult {
        let tmp = TempDir::new().unwrap();
        run_script(
            tmp.path(),
            script,
            test_name,
            &RunConfig::default(),
            &no_overlay_env(tmp.path()),
        )
    }

    #[test]
    fn context_dir_name_flattens_separators_and_stays_unique() {
        let mut taken = std::collections::HashSet::new();
        // Ordinary names (including the `:` and `#` forms main.rs produces)
        // are kept verbatim.
        assert_eq!(context_dir_name("test_foo", &mut taken), "test_foo");
        assert_eq!(
            context_dir_name("a.test:test_foo#2", &mut taken),
            "a.test:test_foo#2"
        );
        // Separators are replaced, so the result is always one component.
        assert_eq!(
            context_dir_name("test_x/../../etc", &mut taken),
            "test_x_.._.._etc"
        );
        assert_eq!(context_dir_name("/etc/passwd", &mut taken), "_etc_passwd");
        // Names that collapse onto each other still get their own dir.
        assert_eq!(context_dir_name("test_a/b", &mut taken), "test_a_b");
        assert_eq!(context_dir_name("test_a_b", &mut taken), "test_a_b_2");
        // Pure traversal components never survive as-is.
        assert_eq!(context_dir_name("..", &mut taken), "_..");
        assert_eq!(context_dir_name("", &mut taken), "_");
    }

    #[test]
    fn context_dir_name_never_escapes_its_parent() {
        let mut taken = std::collections::HashSet::new();
        let parent = Path::new("/run/attest-tmp");
        for name in ["test_x/../../../tmp/pwned", "../../etc", "/etc", "."] {
            let dir = parent.join(context_dir_name(name, &mut taken));
            assert_eq!(dir.parent(), Some(parent), "{name} escaped to {dir:?}");
        }
    }

    /// A test function name containing `/` must not make the runner create,
    /// write to or delete anything outside the run's temp dir, nor make
    /// `--save-context` write outside the directory it was given.
    #[test]
    fn traversing_test_name_cannot_escape_the_save_dir() {
        let tmp = TempDir::new().unwrap();
        let (path, tf) = parse_script(tmp.path(), "test_escape() {\n  echo hi > marker\n}\n");

        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious"), "keep me").unwrap();

        let save_dir = tmp.path().join("save/inner");
        let display = "test_escape/../../../outside";
        let config = RunConfig {
            parallel: 1,
            save_context: Some(save_dir.clone()),
            ..RunConfig::default()
        };

        let results = run_all_tests(
            vec![(display, "test_escape", tf.functions.as_slice(), &*path)],
            &config,
        )
        .unwrap();
        assert!(results[0].passed);
        // The display name is untouched; only the directory derived from it is.
        assert_eq!(results[0].name, display);

        // Nothing was written through the traversal, and nothing was removed.
        assert_eq!(
            fs::read_to_string(outside.join("precious")).unwrap(),
            "keep me"
        );
        let leaked: Vec<_> = fs::read_dir(&outside)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leaked.len(),
            1,
            "run leaked files outside its dirs: {leaked:?}"
        );

        // The saved context landed in one directory directly under save_dir.
        let saved: Vec<PathBuf> = fs::read_dir(&save_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(saved.len(), 1, "unexpected save dir contents: {saved:?}");
        assert!(saved[0].join("stdout.log").exists());
    }

    #[test]
    fn shell_exists_detects_real_and_missing_shells() {
        // Absolute path that exists vs. one that doesn't.
        assert!(shell_exists("/bin/sh"));
        assert!(!shell_exists("/no/such/shell"));
        // Bare name resolved via PATH.
        assert!(shell_exists("sh"));
        assert!(!shell_exists("definitely-not-a-real-shell-xyz"));
    }

    #[test]
    fn resolve_shell_falls_back_when_missing() {
        assert_eq!(resolve_shell("/bin/sh"), "/bin/sh");
        assert_eq!(resolve_shell("/no/such/shell"), "/bin/sh");
    }

    #[test]
    fn execute_passing_test() {
        assert!(run_inline("test_pass() {\n  true\n}\n", "test_pass").passed);
    }

    #[test]
    fn execute_failing_test() {
        assert!(!run_inline("test_fail() {\n  false\n}\n", "test_fail").passed);
    }

    #[test]
    fn execute_test_with_helper() {
        assert!(run_inline(
            "get_value() {\n  echo 42\n}\ntest_helper() {\n  val=$(get_value)\n  test \"$val\" = \"42\"\n}\n",
            "test_helper",
        ).passed);
    }

    #[test]
    fn test_starts_in_clean_cwd() {
        let r = run_inline(
            "test_cwd() {\n  test -z \"$(ls -A .)\"\n  echo data > f.txt\n}\n",
            "test_cwd",
        );
        assert!(r.passed);
        assert!(cwd_dir(&r.context).join("f.txt").exists());
    }

    #[test]
    fn background_process_killed_after_normal_exit() {
        let r = run_inline(
            "test_bg() {\n  sleep 300 &\n  echo $! > pid\n}\n",
            "test_bg",
        );
        assert!(r.passed);
        let pid: u32 = fs::read_to_string(cwd_dir(&r.context).join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The straggler must be dead shortly after the test completes.
        assert!(await_death(pid, 5), "background process survived the test");
    }

    /// Wait (up to `secs`) for `pid` to stop being a live process. A reparented
    /// straggler may linger as a zombie when nothing reaps it (e.g. in minimal
    /// containers), which still counts as killed.
    fn await_death(pid: u32, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match process_state(pid) {
                None | Some('Z') => return true,
                _ if Instant::now() > deadline => return false,
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn test_tree_is_killed_before_the_child_is_reaped() {
        // `kill(-pgid)` is only meaningful while the group leader is unreaped:
        // the leader is the test's child, its pid *is* the pgid, and reaping it
        // releases that number for the kernel to hand to anybody. So the tree
        // has to be killed before the reap, not after — otherwise attest
        // SIGKILLs whichever unrelated process group inherited the number.
        //
        // The observable consequence of the right order: by the time `reap`
        // hands back an exit status, the straggler is already gone. Reaping
        // first leaves it running until the context is dropped.
        let tmp = TempDir::new().unwrap();
        let (path, tf) = parse_script(
            tmp.path(),
            "test_bg() {\n  sleep 300 &\n  echo $! > pid\n}\n",
        );
        let ctx = TempDir::new().unwrap().keep();
        let config = RunConfig::default();
        let mut pending = spawn_test(
            "test_bg",
            "test_bg",
            &tf.functions,
            &path,
            ctx,
            &config,
            &no_overlay_env(tmp.path()),
        )
        .unwrap();

        let status = reap_blocking(&mut pending);
        assert!(status.success());

        let context = pending.context.clone().unwrap();
        let pid: u32 = fs::read_to_string(cwd_dir(&context).join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // `pending` is deliberately still alive, so nothing but the pre-reap
        // kill can account for the straggler being dead.
        assert!(
            await_death(pid, 5),
            "background process outlived the reap: the tree was killed too late"
        );
        drop(pending);
    }

    #[test]
    fn execute_test_stdout_captured() {
        let r = run_inline("test_echo() {\n  echo captured_output\n}\n", "test_echo");
        let stdout = fs::read_to_string(r.context.join("stdout.log")).unwrap();
        assert!(stdout.contains("captured_output"));
    }

    #[test]
    fn execute_test_with_override() {
        // Override `true` (always succeeds) to verify the copy lands in bin/ and runs.
        let tmp = TempDir::new().unwrap();
        let config = RunConfig {
            override_cmds: vec![OverrideSpec {
                name: "true".into(),
                source: which::which("true").unwrap(),
            }],
            ..RunConfig::default()
        };
        let result = run_script(
            tmp.path(),
            "test_override() {\n  true\n}\n",
            "test_override",
            &config,
            &no_overlay_env(tmp.path()),
        );
        assert!(result.passed);
        // bin/true should exist in the context dir
        assert!(result.context.join("bin/true").exists());
    }

    #[test]
    fn execute_test_with_bin_dir() {
        // A directory passed via --bin-dir is prepended to PATH, so a bare-name
        // call to an executable living there resolves (no copy into the context).
        let bin = TempDir::new().unwrap();
        let tool = bin.path().join("mytool");
        fs::write(&tool, "#!/bin/sh\necho mytool_ran\n").unwrap();
        fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let tmp = TempDir::new().unwrap();
        let config = RunConfig {
            bin_dirs: vec![bin.path().to_path_buf()],
            ..RunConfig::default()
        };
        let result = run_script(
            tmp.path(),
            "test_bin_dir() {\n  out=$(mytool)\n  test \"$out\" = \"mytool_ran\"\n}\n",
            "test_bin_dir",
            &config,
            &no_overlay_env(tmp.path()),
        );
        assert!(result.passed);
        // The tool is referenced in place, not copied into the context bin/.
        assert!(!result.context.join("bin/mytool").exists());
    }

    /// A RunEnv with real isolation, or `None` when this environment cannot
    /// mount overlays (the test should then be skipped).
    fn overlay_env(scratch: &Path, invocation_dir: &Path) -> Option<RunEnv> {
        let submounts = overlay::compute_submounts(invocation_dir);
        let mode = overlay::probe_support(scratch, invocation_dir, &submounts)?;
        Some(RunEnv {
            invocation_dir: invocation_dir.to_path_buf(),
            overlay_mode: Some(mode),
            submounts,
        })
    }

    /// Run one test function with full isolation; skip (None) when unsupported.
    fn run_isolated(script: &str, test_name: &str) -> Option<TestResult> {
        let tmp = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let env = overlay_env(tmp.path(), srcdir.path())?;
        Some(run_script(
            srcdir.path(),
            script,
            test_name,
            &RunConfig::default(),
            &env,
        ))
    }

    #[test]
    fn overlay_isolates_root_writes() {
        // The test sees the real root (reads /usr) but its write to a system path
        // must land in the ephemeral upper layer, not on the host filesystem.
        let Some(result) = run_isolated(
            "test_o() {\n  test -d /usr\n  echo made > /attest_root_marker.txt\n}\n",
            "test_o",
        ) else {
            return; // overlays unavailable here
        };
        assert!(result.passed);
        assert!(
            overlay::upper_dir(&result.context)
                .join("attest_root_marker.txt")
                .exists()
        );
        assert!(!Path::new("/attest_root_marker.txt").exists());
    }

    #[test]
    fn overlay_isolates_tmp_writes() {
        // Writes to /tmp must not reach the host: they land in the root upper
        // layer (when /tmp is part of the root fs) or in the upper of /tmp's
        // own ephemeral overlay (when /tmp is a separate mount).
        let marker = "attest_unit_tmp_marker";
        let _ = fs::remove_file(Path::new("/tmp").join(marker));
        let Some(result) = run_isolated(
            &format!("test_t() {{\n  echo made > /tmp/{marker}\n}}\n"),
            "test_t",
        ) else {
            return; // overlays unavailable here
        };
        assert!(result.passed);
        assert!(!Path::new("/tmp").join(marker).exists());

        let mut uppers = vec![overlay::upper_dir(&result.context).join("tmp")];
        for i in 0.. {
            let dir = overlay::submount_upper_dir(&result.context, i);
            if !dir.exists() {
                break;
            }
            uppers.push(dir);
        }
        assert!(
            uppers.iter().any(|u| u.join(marker).exists()),
            "marker not found in any upper layer"
        );
    }

    #[test]
    fn create_strace_wrappers_creates_scripts() {
        // Only run if strace and ls are available
        if which::which("ls").is_err() || which::which("strace").is_err() {
            return;
        }

        let tmp = TempDir::new().unwrap();
        let commands = vec!["ls".to_string()];

        create_strace_wrappers(tmp.path(), &commands).unwrap();

        let wrapper = tmp.path().join("strace_bin/ls");
        assert!(wrapper.exists());

        let content = fs::read_to_string(&wrapper).unwrap();
        assert!(content.starts_with("#!/bin/sh\n"));
        assert!(content.contains("strace"));
        assert!(content.contains("\"$@\""));

        // Check it's executable
        let perms = fs::metadata(&wrapper).unwrap().permissions();
        assert!(perms.mode() & 0o111 != 0);
    }

    #[test]
    fn create_strace_wrappers_unknown_command_errors() {
        if which::which("strace").is_err() {
            return; // the missing-strace check would fire first
        }

        let tmp = TempDir::new().unwrap();
        let err = create_strace_wrappers(tmp.path(), &["nonexistent_cmd_xyz".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonexistent_cmd_xyz"), "got {err}");
    }

    #[test]
    fn create_strace_wrappers_keeps_path_specs_inside_the_context() {
        if which::which("strace").is_err() {
            return;
        }

        // A command given as a path must not be written through: the wrapper
        // belongs in strace_bin/ under the command's base name, and the traced
        // binary must survive untouched.
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim");
        fs::write(&victim, "#!/bin/sh\necho original\n").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();

        let tmp = TempDir::new().unwrap();
        create_strace_wrappers(tmp.path(), &[victim.display().to_string()]).unwrap();

        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "#!/bin/sh\necho original\n",
            "the traced binary was overwritten with the wrapper"
        );
        assert!(!victim.with_extension("log").exists());

        let wrapper = fs::read_to_string(tmp.path().join("strace_bin/victim")).unwrap();
        assert!(wrapper.contains(&victim.display().to_string()), "{wrapper}");
        assert!(
            wrapper.contains(&tmp.path().join("strace/victim.log").display().to_string()),
            "{wrapper}"
        );
    }

    #[test]
    fn save_context_includes_strace_logs() {
        // --strace writes its logs into the context dir, which is a temp dir
        // discarded at exit: --save-context is the only way to read them, so it
        // must copy them out alongside the other logs.
        let contexts = TempDir::new().unwrap();
        // Contexts are named after the test (sanitized; see context_dir_name)
        // and save_test_context reuses that name under the save dir.
        let ctx = contexts.path().join("test_traced");
        fs::create_dir_all(ctx.join("strace")).unwrap();
        fs::write(ctx.join("strace/ls.log"), "execve(\"/bin/ls\")\n").unwrap();
        fs::write(ctx.join("xtrace.log"), "+1: ls\n").unwrap();

        let result = TestResult {
            name: "test_traced".to_string(),
            passed: true,
            timed_out: false,
            duration: Duration::from_millis(1),
            context: ctx,
            source_path: PathBuf::from("t.sh"),
            #[cfg(feature = "cgroup")]
            resources: None,
        };

        let save = TempDir::new().unwrap();
        save_test_context(&result, save.path(), &[]);

        let saved = save.path().join("test_traced");
        assert_eq!(
            fs::read_to_string(saved.join("strace/ls.log")).unwrap(),
            "execve(\"/bin/ls\")\n"
        );
        assert!(saved.join("xtrace.log").exists());
    }

    #[test]
    fn save_context_never_writes_through_symlinks_from_the_delta() {
        // A test may create symlinks anywhere in its ephemeral root; they land
        // in its upper layer and --save-context copies them out verbatim. The
        // rest of the save (logs, cwd, submount deltas) must not then be written
        // *through* those symlinks, or a test could overwrite any file the user
        // running attest can write — straight out of its sandbox.
        let outside = TempDir::new().unwrap();
        let victim_file = outside.path().join("victim.txt");
        fs::write(&victim_file, "original").unwrap();
        let victim_dir = outside.path().join("victim_dir");
        fs::create_dir(&victim_dir).unwrap();

        let contexts = TempDir::new().unwrap();
        let ctx = contexts.path().join("test_evil");
        let upper = overlay::upper_dir(&ctx);
        fs::create_dir_all(&upper).unwrap();
        // The delta of a test that ran `ln -s <host path> /stdout.log` etc.
        std::os::unix::fs::symlink(&victim_file, upper.join("stdout.log")).unwrap();
        std::os::unix::fs::symlink(&victim_dir, upper.join("cwd")).unwrap();
        std::os::unix::fs::symlink(&victim_dir, upper.join("var")).unwrap();

        // The real log, working directory and /var/tmp submount delta, each of
        // which is saved at a name the delta has already claimed.
        fs::write(ctx.join("stdout.log"), "attacker controlled").unwrap();
        fs::create_dir_all(cwd_dir(&ctx)).unwrap();
        fs::write(cwd_dir(&ctx).join("artifact"), "cwd artifact").unwrap();
        let sub_upper = overlay::submount_upper_dir(&ctx, 0);
        fs::create_dir_all(&sub_upper).unwrap();
        fs::write(sub_upper.join("scratch"), "submount artifact").unwrap();

        let result = TestResult {
            name: "test_evil".to_string(),
            passed: true,
            timed_out: false,
            duration: Duration::from_millis(1),
            context: ctx,
            source_path: PathBuf::from("t.sh"),
            #[cfg(feature = "cgroup")]
            resources: None,
        };
        let submounts = [overlay::Submount {
            source: PathBuf::from("/var/tmp"),
            is_dir: true,
            ephemeral: true,
        }];

        let save = TempDir::new().unwrap();
        save_test_context(&result, save.path(), &submounts);

        // Nothing escaped the save dir.
        assert_eq!(fs::read_to_string(&victim_file).unwrap(), "original");
        assert_eq!(
            fs::read_dir(&victim_dir).unwrap().count(),
            0,
            "files were written outside the save dir"
        );

        // ...and everything still landed inside it, as real files.
        let saved = save.path().join("test_evil");
        assert_eq!(
            fs::read_to_string(saved.join("stdout.log")).unwrap(),
            "attacker controlled"
        );
        assert!(
            !fs::symlink_metadata(saved.join("stdout.log"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(saved.join("cwd/artifact")).unwrap(),
            "cwd artifact"
        );
        assert_eq!(
            fs::read_to_string(saved.join("var/tmp/scratch")).unwrap(),
            "submount artifact"
        );
    }

    #[test]
    fn run_all_tests_serial() {
        let tmp = TempDir::new().unwrap();
        let (path, tf) = parse_script(tmp.path(), "test_a() {\n  true\n}\ntest_b() {\n  true\n}\n");
        let config = RunConfig {
            parallel: 1,
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf, &path), &config).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed));
    }

    #[test]
    fn bail_stops_after_first_failure() {
        let tmp = TempDir::new().unwrap();
        // test_fail comes first alphabetically, test_pass second
        let (path, tf) = parse_script(
            tmp.path(),
            "test_fail() {\n  false\n}\ntest_pass() {\n  true\n}\n",
        );
        let config = RunConfig {
            parallel: 1,
            bail: true,
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf, &path), &config).unwrap();
        // Only the failing test ran; bail stopped execution
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
    }

    #[test]
    fn run_all_tests_parallel() {
        let tmp = TempDir::new().unwrap();
        let (path, tf) = parse_script(
            tmp.path(),
            "test_x() {\n  true\n}\ntest_y() {\n  false\n}\n",
        );
        let config = RunConfig {
            parallel: 0,
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf, &path), &config).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r.passed));
        assert!(results.iter().any(|r| !r.passed));
    }

    #[test]
    fn timeout_kills_slow_test() {
        let tmp = TempDir::new().unwrap();
        let (path, tf) = parse_script(tmp.path(), "test_slow() {\n  sleep 60\n}\n");
        let config = RunConfig {
            parallel: 1,
            timeout: Some(std::time::Duration::from_millis(200)),
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf, &path), &config).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert!(results[0].timed_out);
    }

    /// Wait (up to `secs`) for `pid` to reach process state `want`.
    fn await_state(pid: u32, want: char, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match process_state(pid) {
                Some(s) if s == want => return true,
                _ if Instant::now() > deadline => return false,
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn fuzz_pauses_and_resumes_a_descendant() {
        // The baseline the hardening below must not break: a live descendant of
        // a test gets stopped by a pausing tick and resumed by a resuming one.
        let mut shell = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $! >&2; wait"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = String::new();
        {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(shell.stderr.take().unwrap());
            reader.read_line(&mut out).unwrap();
        }
        let sleeper: u32 = out.trim().parse().unwrap();
        assert!(await_state(sleeper, 'S', 5), "sleeper never started");

        // level 1.0 always pauses, 0.0 always resumes.
        let mut rng = 1;
        fuzz_tick(shell.id(), 1.0, &mut rng);
        assert!(await_state(sleeper, 'T', 5), "descendant was not paused");
        fuzz_tick(shell.id(), 0.0, &mut rng);
        assert!(await_state(sleeper, 'S', 5), "descendant was not resumed");

        let _ = shell.kill();
        let _ = shell.wait();
        unsafe { libc::kill(sleeper as libc::pid_t, libc::SIGKILL) };
    }

    #[test]
    fn fuzz_never_signals_a_process_outside_the_test_tree() {
        // pids are reissued as soon as their process is reaped, so a pid read
        // out of a /proc walk may belong to somebody else by the time the fuzz
        // scheduler gets to it. A stray SIGSTOP is unrecoverable — nothing
        // resumes a process that is not in the tree — so a pid whose process is
        // no longer part of the test must not be signalled at all.
        //
        // Detached from the harness's pipes: a process this test leaves stopped
        // must not be able to hold them open and wedge the whole run.
        let mut outsider = Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = outsider.id();
        assert!(await_state(pid, 'S', 5), "outsider never started");

        // An empty tree stands for "this pid is not one of ours": the process
        // exists and is in the right state, and must still be left alone.
        assert!(
            !signal_tree_member(pid, &[], false, libc::SIGSTOP),
            "signalled a process outside the tree"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            process_state(pid),
            Some('S'),
            "an unrelated process was paused"
        );

        // Same pid, now presented as a genuine child of this process: signalled.
        let tree = [std::process::id()];
        assert!(signal_tree_member(pid, &tree, false, libc::SIGSTOP));
        assert!(await_state(pid, 'T', 5), "a tree member was not paused");

        let _ = outsider.kill();
        let _ = outsider.wait();
    }

    #[test]
    fn a_pinned_process_cannot_be_signalled_once_its_pid_is_free() {
        // The property the fix rests on: a /proc/<pid> descriptor is bound to
        // one incarnation of the pid. Once the process is reaped — the moment
        // the kernel may hand the number to an unrelated process — signalling
        // through the descriptor fails instead of reaching the new owner.
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pinned = PinnedProcess::open(child.id()).expect("pin a live child");
        assert!(pinned.stat().is_some());
        assert!(pinned.send_signal(0), "signalling a live process must work");

        let _ = child.kill();
        let _ = child.wait(); // reaped: the pid is now free for reuse

        assert!(pinned.stat().is_none(), "a reaped process still reads back");
        assert!(
            !pinned.send_signal(libc::SIGSTOP),
            "a reaped process was still signalled through its pinned descriptor"
        );
    }

    #[test]
    fn stat_fields_survive_a_hostile_comm() {
        // `comm` is parenthesized but may itself hold spaces and parens, so the
        // state and ppid fields are only found from the *last* ')'.
        let stat = "42 (we (are) (legion) ) T 7 7 0 -1 4194304";
        assert_eq!(stat_state(stat), Some('T'));
        assert_eq!(stat_ppid(stat), Some(7));
        assert_eq!(stat_state("garbage"), None);
        assert_eq!(stat_ppid("42 (sh) T"), None);
    }

    #[test]
    fn functions_source_preserves_line_numbers() {
        // A helper, a blank line, then a test: every function line must land on
        // the same line number it has in the source, with the gaps blanked.
        let source = "helper() {\n  echo setup\n}\n\ntest_foo() {\n  echo a\n  false\n}\n";
        let tmp = TempDir::new().unwrap();
        let (_, tf) = parse_script(tmp.path(), source);

        let generated = build_functions_source(&tf.functions, Some(source));
        let gen_lines: Vec<&str> = generated.lines().collect();
        let src_lines: Vec<&str> = source.lines().collect();

        // Function lines match the source exactly; the separator line is blanked.
        for idx in [0, 1, 2, 4, 5, 6, 7] {
            assert_eq!(gen_lines[idx], src_lines[idx], "line {idx} differs");
        }
        assert_eq!(gen_lines[3], "", "top-level/gap line 3 should be blank");
    }

    #[test]
    fn functions_source_falls_back_when_the_source_is_unavailable() {
        // Without the original text there are no line numbers to preserve, so
        // every function is re-rendered from the AST. Line alignment is lost
        // (and with it the diagnostic snippet), but the script must still be
        // valid shell that defines and runs each function.
        let source = "helper() {\n  echo 42\n}\n\ntest_foo() {\n  test \"$(helper)\" = 42\n}\n";
        let tmp = TempDir::new().unwrap();
        let (_, tf) = parse_script(tmp.path(), source);

        let generated = build_functions_source(&tf.functions, None);
        let script = tmp.path().join("fallback.sh");
        fs::write(&script, &generated).unwrap();

        let status = Command::new("/bin/sh")
            .args(["-c", &format!(". {}\ntest_foo\n", sh_quote(&script))])
            .status()
            .unwrap();
        assert!(status.success(), "generated script failed:\n{generated}");
    }

    #[test]
    fn xtrace_line_number_matches_source() {
        // The failing command sits on source line 7 (1-based). With
        // line-preserving functions.sh, the xtrace PS4 must report `+7:`.
        let source = "helper() {\n  echo setup\n}\n\ntest_foo() {\n  echo a\n  false\n}\n";
        let r = run_inline(source, "test_foo");
        assert!(!r.passed);

        let xtrace = fs::read_to_string(r.context.join("xtrace.log")).unwrap();
        // Last non-subshell trace line is the failing command.
        let last = xtrace
            .lines()
            .rfind(|l| l.starts_with('+') && !l.starts_with("++"))
            .expect("a trace line");
        assert!(
            last.starts_with("+7: "),
            "expected failing command on source line 7, got {last:?}"
        );
    }
}

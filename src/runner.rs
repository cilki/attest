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

/// Tracks live xtrace streaming state: which test holds the output lock and how
/// far we've read into its xtrace.log.
struct XtraceStreamer {
    /// Name of the test currently holding the xtrace output lock.
    holder: Option<String>,
    /// File handle for the current holder's xtrace.log.
    file: Option<std::fs::File>,
    /// How many bytes have been printed so far.
    offset: u64,
}

impl XtraceStreamer {
    fn new() -> Self {
        Self {
            holder: None,
            file: None,
            offset: 0,
        }
    }

    /// Acquire the lock for a test if no one currently holds it.
    fn try_acquire(&mut self, pending: &PendingTest) {
        if self.holder.is_some() {
            return;
        }
        let xtrace_path = pending.context.as_ref().unwrap().join("xtrace.log");
        if let Ok(f) = std::fs::File::open(&xtrace_path) {
            eprintln!("\x1b[2m--- xtrace: {} ---\x1b[0m", pending.name);
            self.holder = Some(pending.name.clone());
            self.file = Some(f);
            self.offset = 0;
        }
    }

    /// Check whether the current holder's xtrace.log has new data.
    fn has_new(&mut self) -> bool {
        let Some(ref mut f) = self.file else {
            return false;
        };
        f.metadata().is_ok_and(|m| m.len() > self.offset)
    }

    /// Print any new bytes from the current holder's xtrace.log.
    fn flush_new(&mut self) {
        let Some(ref mut f) = self.file else { return };
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
            self.offset += buf.len() as u64;
            let _ = write!(std::io::stderr(), "\x1b[2m");
            let _ = std::io::stderr().write_all(&buf);
            let _ = write!(std::io::stderr(), "\x1b[0m");
        }
    }

    /// Release the lock (flush remaining output first).
    fn release(&mut self) {
        self.flush_new();
        self.holder = None;
        self.file = None;
        self.offset = 0;
    }

    /// Check if the named test currently holds the lock.
    fn is_holder(&self, name: &str) -> bool {
        self.holder.as_deref() == Some(name)
    }

    /// Dump the full xtrace log for a test that was never streamed (e.g. it
    /// finished before the parent could open the file).
    fn dump_missed(&self, pending: &PendingTest) {
        if self.is_holder(&pending.name) {
            return; // Will be flushed via release()
        }
        dump_xtrace_log(&pending.name, pending.context.as_ref().unwrap());
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
    fn kill_tree(&mut self) {
        #[cfg(feature = "cgroup")]
        if let Some(ref cg) = self.cgroup {
            cg.kill_all();
        }
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.kill();
    }
}

impl Drop for PendingTest {
    fn drop(&mut self) {
        // Always kill the tree: even when the main shell exited normally,
        // background processes it spawned survive in its session/cgroup, and
        // tests are promised they never need to clean those up themselves.
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
    /// When set, each test's context directory is created here and left on exit.
    /// When unset, context dirs are temporary and cleaned up automatically.
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

pub fn run_all_tests(
    tests: Vec<(&str, &str, &[FunctionDefinition], &Path)>,
    config: &RunConfig,
) -> Result<Vec<TestResult>> {
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

    // Spawn one test from an iterator tuple, deriving its context dir from the
    // unique display name. Used to seed the initial batch and to refill a slot
    // as each test completes.
    let spawn = |(display_name, fn_name, all_functions, source_path): (
        &str,
        &str,
        &[FunctionDefinition],
        &Path,
    )| {
        spawn_test(
            display_name,
            fn_name,
            all_functions,
            source_path,
            contexts_dir.join(display_name),
            config,
            &env,
        )
    };

    // Seed the initial batch up to max_parallel.
    while pending_list.len() < max_parallel {
        match test_iter.next() {
            Some(test) => pending_list.push(spawn(test)?),
            None => break,
        }
    }

    // If xtrace is enabled, acquire the lock for the first pending test.
    if let Some(ref mut xt) = xtrace
        && let Some(p) = pending_list.first()
    {
        status.suspend(|| xt.try_acquire(p));
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

        // Randomly pause/resume individual descendant processes to introduce timing fuzziness.
        // Each tick either pauses one random running process OR resumes one random stopped
        // process — never both.
        if let Some(fuzz_level) = config.fuzz {
            for pending in pending_list.iter_mut() {
                let roll = xorshift64(&mut rng);
                let test_pid = pending.child.id();
                let descendants: Vec<u32> = collect_descendants(test_pid)
                    .into_iter()
                    .filter(|&pid| pid != test_pid)
                    .collect();
                if (roll as f64) / (u64::MAX as f64) >= fuzz_level {
                    // Resume a random stopped descendant.
                    let stopped: Vec<u32> = descendants
                        .iter()
                        .copied()
                        .filter(|&pid| process_state(pid) == Some('T'))
                        .collect();
                    if !stopped.is_empty() {
                        let chosen = stopped[(xorshift64(&mut rng) as usize) % stopped.len()];
                        if unsafe { libc::kill(chosen as libc::pid_t, libc::SIGCONT) } == 0 {
                            trace!(pid = chosen, "Resumed subprocess");
                        }
                    }
                } else {
                    // Pause a random running descendant.
                    let running: Vec<u32> = descendants
                        .iter()
                        .copied()
                        .filter(|&pid| process_state(pid) != Some('T'))
                        .collect();
                    if !running.is_empty() {
                        let chosen = running[(xorshift64(&mut rng) as usize) % running.len()];
                        if unsafe { libc::kill(chosen as libc::pid_t, libc::SIGSTOP) } == 0 {
                            trace!(pid = chosen, "Paused subprocess");
                        }
                    }
                }
            }
        }

        // Non-blocking reap: check all pending children.
        let mut reaped: Vec<(usize, ExitStatus)> = Vec::new();
        for (i, pending) in pending_list.iter_mut().enumerate() {
            match pending.child.try_wait() {
                Ok(Some(status)) => reaped.push((i, status)),
                Ok(None) => {} // still running
                Err(e) => return Err(anyhow!("try_wait failed: {e}")),
            }
        }

        // Process reaped tests in reverse index order so removal doesn't shift indices.
        reaped.sort_by_key(|b| std::cmp::Reverse(b.0));
        let mut completed: Vec<TestResult> = Vec::new();
        for (i, exit_status) in reaped {
            if let Some(ref mut xt) = xtrace {
                // If this test held the xtrace lock, release it (flushes remaining output).
                // Otherwise dump the full log for tests that finished before we could stream.
                status.suspend(|| {
                    if xt.is_holder(&pending_list[i].name) {
                        xt.release();
                    } else {
                        xt.dump_missed(&pending_list[i]);
                    }
                });
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
            && xt.holder.is_none()
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

/// Resolve a shell name or path to an executable, falling back to `/bin/sh`
/// when the requested shell is not found.
/// Whether `shell` names something we can exec: an existing path when it
/// contains a `/`, or a command resolvable on `PATH` otherwise.
pub(crate) fn shell_exists(shell: &str) -> bool {
    if shell.contains('/') {
        std::path::Path::new(shell).exists()
    } else {
        which::which(shell).is_ok()
    }
}

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
fn build_functions_source(functions: &[FunctionDefinition], source: &str) -> String {
    let src_lines: Vec<&str> = source.lines().collect();
    let mut out = String::new();
    // 1-based line number of the next line to be written to `out`.
    let mut line = 1usize;

    for func in functions {
        let Some(span) = func.location() else {
            // No source location: fall back to the reformatted rendering. This
            // breaks line alignment for this one function but keeps it runnable.
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
    // original line numbers (see build_functions_source). Fall back to the
    // reformatted AST rendering if the file can't be read.
    let script = match std::fs::read_to_string(source_path) {
        Ok(source) => build_functions_source(all_functions, &source),
        Err(_) => {
            let mut s = String::new();
            for func in all_functions {
                s.push_str(&func.to_string());
                s.push('\n');
            }
            s
        }
    };
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
/// into `<save_dir>/<test name>` for `--save-context`. The temp context dir is
/// otherwise discarded when the run's tempdir is dropped.
fn save_test_context(result: &TestResult, save_dir: &Path, submounts: &[overlay::Submount]) {
    let dst = save_dir.join(&result.name);
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
        if let Err(e) = overlay::copy_dir_recursive(&sub_upper, &dst.join(rel)) {
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
    for log in ["stdout.log", "xtrace.log"] {
        let src = result.context.join(log);
        if src.exists() {
            let _ = std::fs::copy(&src, dst.join(log));
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
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // Format: "pid (comm) state ..."  — comm may contain spaces/parens, so
    // find the *last* ')' to reliably locate the state field.
    let after_comm = stat.rfind(')')?.checked_add(2)?;
    stat[after_comm..].chars().next()
}

fn create_strace_wrappers(working_dir: &Path, commands: &[String]) -> Result<()> {
    let strace_bin = working_dir.join("strace_bin");
    std::fs::create_dir_all(&strace_bin)?;

    let strace_dir = working_dir.join("strace");
    std::fs::create_dir_all(&strace_dir)?;

    for cmd in commands {
        let real_path =
            which::which(cmd).map_err(|_| anyhow!("--strace: command not found: {cmd}"))?;

        let wrapper = strace_bin.join(cmd);
        let strace_out = strace_dir.join(format!("{cmd}.log"));
        let script = format!(
            "#!/bin/sh\nexec strace -f -o {} {} \"$@\"\n",
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

    /// Blocking wait + build result, for tests only.
    fn wait_and_collect(mut pending: PendingTest) -> TestResult {
        let status = pending.child.wait().expect("wait failed");
        build_result(pending, status)
    }

    /// A RunEnv with isolation disabled, for direct spawn_test tests.
    fn no_overlay_env(invocation_dir: &Path) -> RunEnv {
        RunEnv {
            invocation_dir: invocation_dir.to_path_buf(),
            overlay_mode: None,
            submounts: Vec::new(),
        }
    }

    /// Parse `script` content and run `test_name` via spawn_test + wait_and_collect.
    fn run_inline(script: &str, test_name: &str) -> TestResult {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.sh");
        fs::write(&path, script).unwrap();
        let tf = crate::parser::parse_test_file(&path).unwrap();
        let ctx = TempDir::new().unwrap().keep();
        let pending = spawn_test(
            test_name,
            test_name,
            &tf.functions,
            &path,
            ctx,
            &RunConfig::default(),
            &no_overlay_env(tmp.path()),
        )
        .unwrap();
        wait_and_collect(pending)
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
        // The straggler must be dead shortly after the test completes. It may
        // linger as a zombie when nothing reaps reparented children (e.g. in
        // minimal containers), which still counts as killed.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match process_state(pid) {
                None | Some('Z') => break,
                _ if Instant::now() > deadline => panic!("background process survived the test"),
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
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
        let script_content = "test_override() {\n  true\n}\n";
        let path = tmp.path().join("t.sh");
        fs::write(&path, script_content).unwrap();
        let tf = crate::parser::parse_test_file(&path).unwrap();
        let ctx = TempDir::new().unwrap().keep();
        let spec = OverrideSpec {
            name: "true".into(),
            source: which::which("true").unwrap(),
        };
        let config = RunConfig {
            override_cmds: vec![spec],
            ..RunConfig::default()
        };
        let pending = spawn_test(
            "test_override",
            "test_override",
            &tf.functions,
            &path,
            ctx,
            &config,
            &no_overlay_env(tmp.path()),
        )
        .unwrap();
        let result = wait_and_collect(pending);
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
        let path = tmp.path().join("t.sh");
        fs::write(
            &path,
            "test_bin_dir() {\n  out=$(mytool)\n  test \"$out\" = \"mytool_ran\"\n}\n",
        )
        .unwrap();
        let tf = crate::parser::parse_test_file(&path).unwrap();
        let ctx = TempDir::new().unwrap().keep();
        let config = RunConfig {
            bin_dirs: vec![bin.path().to_path_buf()],
            ..RunConfig::default()
        };
        let pending = spawn_test(
            "test_bin_dir",
            "test_bin_dir",
            &tf.functions,
            &path,
            ctx,
            &config,
            &no_overlay_env(tmp.path()),
        )
        .unwrap();
        let result = wait_and_collect(pending);
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
        let path = srcdir.path().join("t.sh");
        fs::write(&path, script).unwrap();
        let tf = crate::parser::parse_test_file(&path).unwrap();
        let ctx = TempDir::new().unwrap().keep();
        let pending = spawn_test(
            test_name,
            test_name,
            &tf.functions,
            &path,
            ctx,
            &RunConfig::default(),
            &env,
        )
        .unwrap();
        Some(wait_and_collect(pending))
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
        if which::which("ls").is_err() {
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
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::metadata(&wrapper).unwrap().permissions();
        assert!(perms.mode() & 0o111 != 0);
    }

    #[test]
    fn create_strace_wrappers_unknown_command_errors() {
        let tmp = TempDir::new().unwrap();
        let result = create_strace_wrappers(tmp.path(), &["nonexistent_cmd_xyz".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn run_all_tests_serial() {
        let tmp = TempDir::new().unwrap();
        let script_content = "test_a() {\n  true\n}\ntest_b() {\n  true\n}\n";

        // Parse to get real FunctionDefinitions
        let path = tmp.path().join("test.sh");
        fs::write(&path, script_content).unwrap();
        let test_file = crate::parser::parse_test_file(&path).unwrap();

        let config = RunConfig {
            parallel: 1,
            ..RunConfig::default()
        };

        let test_refs: Vec<(&str, &str, &[FunctionDefinition], &Path)> = test_file
            .tests
            .iter()
            .map(|t| {
                (
                    t.name.as_str(),
                    t.name.as_str(),
                    test_file.functions.as_slice(),
                    path.as_path(),
                )
            })
            .collect();

        let results = run_all_tests(test_refs, &config).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed));
    }

    #[test]
    fn bail_stops_after_first_failure() {
        let tmp = TempDir::new().unwrap();
        // test_fail comes first alphabetically, test_pass second
        let script_content = "test_fail() {\n  false\n}\ntest_pass() {\n  true\n}\n";

        let path = tmp.path().join("test.sh");
        fs::write(&path, script_content).unwrap();
        let test_file = crate::parser::parse_test_file(&path).unwrap();

        let config = RunConfig {
            parallel: 1,
            bail: true,
            ..RunConfig::default()
        };

        let test_refs: Vec<(&str, &str, &[FunctionDefinition], &Path)> = test_file
            .tests
            .iter()
            .map(|t| {
                (
                    t.name.as_str(),
                    t.name.as_str(),
                    test_file.functions.as_slice(),
                    path.as_path(),
                )
            })
            .collect();

        let results = run_all_tests(test_refs, &config).unwrap();
        // Only the failing test ran; bail stopped execution
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
    }

    #[test]
    fn run_all_tests_parallel() {
        let tmp = TempDir::new().unwrap();
        let script_content = "test_x() {\n  true\n}\ntest_y() {\n  false\n}\n";

        let path = tmp.path().join("test.sh");
        fs::write(&path, script_content).unwrap();
        let test_file = crate::parser::parse_test_file(&path).unwrap();

        let config = RunConfig {
            parallel: 0,
            ..RunConfig::default()
        };

        let test_refs: Vec<(&str, &str, &[FunctionDefinition], &Path)> = test_file
            .tests
            .iter()
            .map(|t| {
                (
                    t.name.as_str(),
                    t.name.as_str(),
                    test_file.functions.as_slice(),
                    path.as_path(),
                )
            })
            .collect();

        let results = run_all_tests(test_refs, &config).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r.passed));
        assert!(results.iter().any(|r| !r.passed));
    }

    #[test]
    fn timeout_kills_slow_test() {
        let tmp = TempDir::new().unwrap();
        let script_content = "test_slow() {\n  sleep 60\n}\n";

        let path = tmp.path().join("test.sh");
        fs::write(&path, script_content).unwrap();
        let tf = crate::parser::parse_test_file(&path).unwrap();

        let config = RunConfig {
            parallel: 1,
            timeout: Some(std::time::Duration::from_millis(200)),
            ..RunConfig::default()
        };

        let test_refs: Vec<(&str, &str, &[FunctionDefinition], &Path)> = tf
            .tests
            .iter()
            .map(|t| {
                (
                    t.name.as_str(),
                    t.name.as_str(),
                    tf.functions.as_slice(),
                    path.as_path(),
                )
            })
            .collect();

        let results = run_all_tests(test_refs, &config).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert!(results[0].timed_out);
    }

    #[test]
    fn functions_source_preserves_line_numbers() {
        // A helper, a blank line, then a test: every function line must land on
        // the same line number it has in the source, with the gaps blanked.
        let source = "helper() {\n  echo setup\n}\n\ntest_foo() {\n  echo a\n  false\n}\n";
        let tf = {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("t.sh");
            fs::write(&path, source).unwrap();
            crate::parser::parse_test_file(&path).unwrap()
        };

        let generated = build_functions_source(&tf.functions, source);
        let gen_lines: Vec<&str> = generated.lines().collect();
        let src_lines: Vec<&str> = source.lines().collect();

        // Function lines match the source exactly; the separator line is blanked.
        for idx in [0, 1, 2, 4, 5, 6, 7] {
            assert_eq!(gen_lines[idx], src_lines[idx], "line {idx} differs");
        }
        assert_eq!(gen_lines[3], "", "top-level/gap line 3 should be blank");
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

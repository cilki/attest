use anyhow::{Result, anyhow};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

use crate::decorator;
use crate::output;
use crate::output::errln;
use crate::overlay;
use crate::seccomp;
use crate::spawn::{Child, Spawn};

/// Set by the SIGINT/SIGTERM handler. Tests run in their own sessions (see the
/// `setsid` hook in `spawn_test`), so the terminal no longer delivers ^C to
/// them directly; the poll loop watches this flag and tears the run down.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn mark_interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// How long the poll loop waits before going round again when nothing else
/// wakes it. It is the cadence of the work that is driven by the clock rather
/// than by a test finishing: `--timeout` expiry, `--fuzz` ticks, the live
/// xtrace tail and the status line.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Blocks `SIGCHLD` for the length of a run so the poll loop can *wait* for a
/// test to finish instead of sleeping [`POLL_INTERVAL`] and asking afterwards.
///
/// Sleeping made that interval a floor under every test: one that finished in a
/// millisecond was still only noticed — and its successor only spawned — on the
/// next tick, so a suite of fast tests cost 50ms per test per worker on top of
/// what the tests themselves took. With the signal blocked, an exiting child
/// leaves a `SIGCHLD` pending, a `signalfd` for it becomes readable, and
/// [`ChildWaiter::wait`] returns as soon as it does; the timeout stays so the
/// clock-driven work above keeps its cadence.
///
/// A `signalfd` rather than `sigtimedwait` because the wait has other
/// descriptors to watch in the same call: every running test with a
/// `syscalls` condition has a seccomp listener, and a syscall the test is
/// blocked in until its notification is answered must not wait for the next
/// tick either. So the wait is one `poll` over the signal and the listeners,
/// answering notifications as they come and returning on a child's exit.
///
/// Blocking is per-thread and inherited by threads started afterwards, so this
/// has to be armed before the run starts anything else — the status bar's
/// steady-tick thread being the one in play here. A thread that does not block
/// `SIGCHLD` can take the delivery instead, and the default disposition
/// discards it; that only costs the early wakeup, since the wait still ends on
/// its timeout, but it would cost it for the whole run.
struct ChildWaiter {
    /// The mask to put back when the run is over.
    previous: libc::sigset_t,
    /// Readable whenever a `SIGCHLD` is pending. `None` when the signal could
    /// not be blocked (or the descriptor not made), in which case waiting for
    /// it would race with its delivery and [`Self::wait`] polls the listeners
    /// alone, on the timer — exactly what the loop did before.
    sigfd: Option<std::os::fd::OwnedFd>,
}

impl ChildWaiter {
    fn new() -> Self {
        // Zeroed rather than `MaybeUninit`: an all-zero `sigset_t` is the empty
        // set, so the value is valid even if `sigemptyset` were to fail.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        let armed = unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous) == 0
        };
        let sigfd = if armed {
            let fd = unsafe { libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
            if fd < 0 {
                // Put the mask back: a blocked signal nobody waits for is
                // worse than the race.
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut())
                };
                None
            } else {
                Some(unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) })
            }
        } else {
            None
        };
        if sigfd.is_none() {
            debug!("could not wait on SIGCHLD; the poll loop will wait on its timer alone");
        }
        Self { previous, sigfd }
    }

    /// Wait until a test child exits or `timeout` elapses, whichever happens
    /// first, answering the `listeners` meanwhile.
    fn wait(&self, timeout: Duration, listeners: &mut [&mut seccomp::Listener]) {
        let deadline = Instant::now() + timeout;
        loop {
            let mut fds: Vec<libc::pollfd> = Vec::with_capacity(listeners.len() + 1);
            // The signal first, so its slot is known.
            fds.push(libc::pollfd {
                fd: self.sigfd.as_ref().map_or(-1, |fd| fd.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.extend(listeners.iter().map(|l| libc::pollfd {
                // A dead listener polls as `-1`, which `poll` ignores.
                fd: l.poll_fd().unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            }));
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            let n = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    left.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if n <= 0 {
                // Nothing within the interval — the ordinary case while
                // tests are running — or a handled signal (^C) cut the wait
                // short. Either way the loop goes round and looks for itself.
                return;
            }
            for (pfd, listener) in fds[1..].iter().zip(listeners.iter_mut()) {
                if pfd.revents != 0 {
                    listener.service();
                }
            }
            if fds[0].revents != 0 {
                // A child exited. Take the signal so the descriptor is quiet
                // again; nothing in it is needed, the reap finds out which.
                let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
                unsafe {
                    libc::read(
                        fds[0].fd,
                        (&mut info as *mut libc::signalfd_siginfo).cast(),
                        std::mem::size_of::<libc::signalfd_siginfo>(),
                    )
                };
                return;
            }
        }
    }
}

impl Drop for ChildWaiter {
    fn drop(&mut self) {
        if self.sigfd.is_some() {
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
            };
        }
    }
}

/// The test whose xtrace.log is currently being tailed to stderr.
///
/// Live xtrace streaming (`-vv`) gives this to one test at a time, so
/// concurrent traces never interleave; the run loop holds `Option<Tailed>`,
/// `None` meaning nobody has it. A test that finishes without ever having
/// held it gets its whole log dumped instead (see [`finish_xtrace`]).
///
/// Whether the run streams at all is `config.verbose >= 2`, read from there
/// by the two sites that hand the tail out and take it back. Recording it a
/// second time as well — a `Some` streamer wrapped around this one `Option` —
/// made every site unwrap two layers to reach the question it was asking, and
/// left the run loop holding a copy of a flag it is given, next to the
/// `config.verbose < 2` the failure report reads straight off `config`.
struct Tailed {
    /// Name of the test holding the xtrace output lock.
    name: String,
    file: std::fs::File,
    /// How many bytes have been printed so far.
    offset: u64,
    /// Another test's trace or result has been printed since this trace last
    /// was, so the next bytes of it need their header again: without one they
    /// read as the tail of whatever was printed in between.
    interrupted: bool,
}

impl Tailed {
    /// Start tailing `pending`'s xtrace.log. `None` when it cannot be opened
    /// yet: the test's own shell creates the file as it starts up, so a
    /// freshly spawned test may not have one for the first tick or two.
    fn open(pending: &PendingTest) -> Option<Self> {
        let xtrace_path = pending.context.as_ref().unwrap().join("xtrace.log");
        let file = overlay::open_nofollow(&xtrace_path).ok()?;
        xtrace_header(&pending.name);
        Some(Self {
            name: pending.name.clone(),
            file,
            offset: 0,
            interrupted: false,
        })
    }

    /// Check whether the tailed xtrace.log has grown since the last flush.
    fn has_new(&self) -> bool {
        self.file.metadata().is_ok_and(|m| m.len() > self.offset)
    }

    /// Print any bytes appended to the tailed xtrace.log since the last flush.
    ///
    /// `last` says there will be no more: the test has finished, so whatever is
    /// in the file is all of it. Until then a trailing [`output::C1_LEAD`] is
    /// left unconsumed and re-read next time (see [`output::replay_log_err`]).
    fn flush_new(&mut self, last: bool) {
        self.flush_new_via(last, output::replay_log_err);
    }

    /// [`Tailed::flush_new`], replaying the new bytes through `replay` rather
    /// than onto the process's stderr. The unit tests replay into a buffer:
    /// whether stderr still has a reader is up to whatever runs the test
    /// binary, and once it has none [`output::output_closed`] latches for the
    /// whole process and every replay after it reads nothing.
    fn flush_new_via(&mut self, last: bool, replay: impl FnOnce(&mut std::fs::File, bool) -> u64) {
        if self.interrupted && self.has_new() {
            xtrace_header(&self.name);
            self.interrupted = false;
        }
        if self.file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        self.offset += replay(&mut self.file, last);
    }
}

/// Settle a finished test's trace output under `-vv`: flush and release the
/// tail if this was the test holding it, otherwise dump its whole log (it
/// never got the tail, because another test held it or because it finished
/// before the parent could open the file).
///
/// The run loop calls this as it prints the test's result rather than as it
/// reaps the test, so the trace lands directly above the `PASS`/`FAIL` line it
/// belongs to: several tests finish in one tick whenever they are fast, and
/// settling them all at reap time stacked every trace of the batch above all
/// of its results.
///
/// Whatever is printed for a test that is not the holder lands in the middle
/// of the holder's trace, so the holder is marked [`Tailed::interrupted`] and
/// announces itself again before its next line.
fn finish_xtrace(tailed: &mut Option<Tailed>, name: &str, context: &Path) {
    match tailed {
        Some(t) if t.name == name => {
            t.flush_new(true);
            *tailed = None;
        }
        _ => {
            dump_xtrace_log(name, context);
            if let Some(t) = tailed {
                t.interrupted = true;
            }
        }
    }
}

/// The `--- xtrace: <name> ---` header a trace is announced by, whether it is
/// being tailed live or dumped whole.
///
/// The name is the test's own, so it goes out escaped: a shell takes almost
/// anything as a function name, and raw those bytes are terminal commands (see
/// [`output::escape_label`]).
fn xtrace_header(name: &str) {
    let output::Palette { dim, reset, .. } = output::err_palette();
    errln!("{dim}--- xtrace: {} ---{reset}", output::escape_label(name));
}

/// Print a test's full xtrace.log, dimmed, under an [`xtrace_header`]. Silent
/// if the log is missing or empty.
///
/// The log is whatever the test and the programs it ran wrote to stderr, so it
/// goes out through [`output::replay_log_err`] rather than being replayed
/// verbatim into the terminal, and a chunk at a time rather than all at once —
/// a failing test's log can be arbitrarily large.
fn dump_xtrace_log(name: &str, context: &Path) {
    // Open the file the runner created, not whatever a symlink left at that
    // name points at: the test can rewrite its own context dir
    // ([`overlay::open_nofollow`]).
    let Ok(mut log) = overlay::open_nofollow(&context.join("xtrace.log")) else {
        return;
    };
    // Nothing to show for a test that wrote nothing to stderr. `open_nofollow`
    // has established that this is a regular file, so its length is the whole
    // of what the replay below would print.
    if !log.metadata().is_ok_and(|m| m.len() > 0) {
        return;
    }
    xtrace_header(name);
    output::replay_log_err(&mut log, true);
}

/// How a test ended. A timeout is a failure, but a distinguishable one: it
/// gets its own verdict in the report, its own `status` in `--json`, and a
/// snippet that points at where the test still was rather than at a command
/// that returned nonzero.
///
/// One field rather than a `passed`/`timed_out` pair, which could also spell
/// the fourth thing a test can never be — timed out *and* passed — and left
/// every reader of a result to re-derive these three cases for itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Pass,
    Fail,
    Timeout,
}

impl Outcome {
    /// Did the test pass? The two ways of not passing are told apart by the
    /// report, not by anything that only has to count failures.
    pub fn passed(self) -> bool {
        self == Outcome::Pass
    }
}

pub struct TestResult<'a> {
    pub name: String,
    /// The shell function's name as written in the file — what `name` is
    /// derived from, and what the file's decorators are keyed by.
    pub test: &'a str,
    /// The parse the test was run from: the failure report renders its
    /// snippet out of the script this holds, and `--add-decorators` finds the
    /// test's definition line and existing blocks in it.
    pub file: &'a crate::parser::TestFile,
    pub outcome: Outcome,
    pub duration: Duration,
    pub context: PathBuf,
    pub resources: Option<crate::cgroup::ResourceStats>,
    /// The syscalls the run watched the test for, and how often each was
    /// made. `None` for a test with no `syscalls` condition — nothing is
    /// watched that nothing asks about — or where the kernel cannot watch.
    pub syscalls: Option<crate::decorator::SyscallLog>,
    /// The test's `#attest(...)` conditions that did not hold. Only a test
    /// whose script succeeded has its conditions checked, so a non-empty list
    /// is the whole reason the outcome is [`Outcome::Fail`] — the commands
    /// all ran, and the report says so by pointing at the condition rather
    /// than at the last of them (see [`crate::diagnostics::print_violations`]).
    /// Under `--update-decorators` the outcome stays [`Outcome::Pass`] and the
    /// list is what gets rewritten (see [`crate::decorate`]).
    pub violations: Vec<crate::decorator::Violation>,
}

impl TestResult<'_> {
    /// Did every command of the test succeed, whatever its decorators said?
    /// Decides whether the failure report replays the trace and blames a
    /// command — both of which would mislead for a test that failed only
    /// a condition on its run.
    pub fn commands_succeeded(&self) -> bool {
        self.outcome == Outcome::Pass || !self.violations.is_empty()
    }
}

/// State held by the parent for a spawned test child that has not yet been
/// waited on. Dropping this kills the child (if still running) and cleans up.
struct PendingTest<'a> {
    child: Child,
    /// Set to `true` when the child was killed due to exceeding `--timeout`.
    timed_out: bool,
    name: String,
    start: Instant,
    /// `None` after the path has been transferred to `TestResult`.
    context: Option<PathBuf>,
    /// The parse the test was run from: the failure report renders its
    /// snippet out of the script this holds (see
    /// [`crate::diagnostics::print_failure_snippet`]), and the function's
    /// decorators are looked up in it by `test`.
    file: &'a crate::parser::TestFile,
    /// The shell function's name, as written in the file (the display name
    /// may be qualified and numbered).
    test: &'a str,
    cgroup: Option<crate::cgroup::TestCgroup>,
    /// Where the test's syscalls are heard, for a test with a `syscalls`
    /// condition on a kernel with seccomp user notification. Serviced by
    /// the poll loop ([`ChildWaiter::wait`]) for as long as the test runs.
    listener: Option<seccomp::Listener>,
}

impl PendingTest<'_> {
    /// Kill the test's entire process tree: the child was made a session (and
    /// process-group) leader at spawn, so its pgid is its pid. The cgroup, when
    /// present, also catches processes that re-`setsid`'d themselves.
    ///
    /// Both of those are only meaningful while the child is unreaped: once it
    /// has been waited on the kernel may hand its pid — and therefore this
    /// pgid — to an unrelated process, and `kill(-pgid)` would SIGKILL
    /// somebody else's process group. So pid-based signalling stops at that
    /// point and only `cgroup.kill`, which is keyed to a directory and can
    /// never be misdirected, still runs.
    fn kill_tree(&mut self) {
        if let Some(ref cg) = self.cgroup {
            cg.kill_all();
        }
        if self.child.reaped() {
            return;
        }
        // The group is led by the child itself, so this kills it too.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
    }

    /// Reap the child if it has exited, killing its process tree first.
    /// `None` while it is still running.
    ///
    /// The order matters: the test's pgid is the child's pid, and that number
    /// stays reserved only until the child is reaped. [`Child::has_exited`]
    /// leaves the zombie in place, so killing the group first and consuming
    /// the zombie second is what makes `kill(-pgid)` provably land on this
    /// test and nothing else.
    fn reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if !self.child.has_exited()? {
            return Ok(None);
        }
        // Background processes the test left behind outlive the shell;
        // tests are promised they never have to clean those up themselves.
        self.kill_tree();
        self.child.wait().map(Some)
    }
}

impl Drop for PendingTest<'_> {
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
    /// A violated `#attest(...)` condition is recorded but does not fail the
    /// test: the run is there to find out what the bounds should be, and
    /// `decorate::plan_updates` rewrites them from the violations afterwards.
    pub update_decorators: bool,
}

/// Run-wide isolation state shared by every spawned test.
struct RunEnv {
    /// Directory attest was invoked from; each test starts here, inside its
    /// ephemeral root.
    invocation_dir: PathBuf,
    /// How this run mounts its overlays — settled by the probe before the
    /// first test is spawned. There is no running without one: a test that
    /// cannot be isolated is not run at all.
    overlay_mode: overlay::Mode,
    /// How each mount under `/` is re-established inside per-test roots.
    submounts: Vec<overlay::Submount>,
    /// Whether this kernel answers seccomp user notifications
    /// (`seccomp::probe`), and so whether a `syscalls` condition can be
    /// watched at all.
    seccomp: bool,
}

/// One test to run: the name it is reported under, the shell function to
/// invoke, and the file both came from.
///
/// The file is borrowed whole rather than taken apart into "the functions" and
/// "the path": those are two halves of the same parse, every test in a file
/// shares them, and a run may hold several `TestSpec`s per file (one per
/// `--repeat`).
pub struct TestSpec<'a> {
    /// Unique within the run: results, per-test context dirs and
    /// `--save-context` dirs are all keyed by it.
    pub display: String,
    /// Name of the shell function to invoke, as written in the file.
    pub name: &'a str,
    pub file: &'a crate::parser::TestFile,
}

pub fn run_all_tests<'a>(
    tests: Vec<TestSpec<'a>>,
    config: &RunConfig,
) -> Result<Vec<TestResult<'a>>> {
    // First thing in the run: the block has to be in place before any thread
    // that could take a `SIGCHLD` delivery instead of leaving it pending — the
    // status bar's ticker, below, is one (see `ChildWaiter`).
    let waiter = ChildWaiter::new();

    let mut results = Vec::new();
    let total = tests.len();
    let mut status = output::StatusDisplay::new(total, config.json);
    let wall_start = Instant::now();

    let max_parallel = config.parallel.max(1);
    let mut test_iter = tests.into_iter();
    let mut pending_list: Vec<PendingTest<'a>> = Vec::new();
    let mut bail_flag = false;
    let mut rng: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xdeadbeef_cafebabe);
    // Whichever test currently has its xtrace streamed to the terminal, under
    // `-vv`. Nothing else records that `-vv` was asked for: every site below
    // reads `config.verbose` for itself, the way the failure report does.
    let mut tailed: Option<Tailed> = None;

    let tmp = private_temp_dir()?;

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
    // Where no mode works the run ends here: a test is promised that its
    // writes are discarded, and running it anyway would break that promise
    // against whatever the user's filesystem holds.
    let invocation_dir = std::env::current_dir()?;
    let submounts = overlay::compute_submounts(&invocation_dir);
    let overlay_mode = overlay::probe_support(tmp.path(), &invocation_dir, &submounts)
        .map_err(|e| anyhow!("cannot isolate tests: {e}"))?;
    match overlay_mode {
        overlay::Mode::Userns => {
            debug!("using unprivileged overlay; tests run as root inside their namespace")
        }
        overlay::Mode::Privileged => debug!("using privileged overlay isolation"),
    }
    let seccomp = match seccomp::probe() {
        Ok(()) => true,
        Err(why) => {
            debug!("seccomp user notification unavailable: {why}");
            false
        }
    };
    let env = RunEnv {
        invocation_dir,
        overlay_mode,
        submounts,
        seccomp,
    };

    // Contexts always live in the temp dir; `--save-context` copies each test's
    // upper layer and logs out afterward (see save_test_context).
    let contexts_dir = tmp.path();
    if let Some(ref save_dir) = config.save_context {
        std::fs::create_dir_all(save_dir)?;
    }

    let mut context_names = std::collections::HashSet::new();
    let mut spawn = |spec: TestSpec<'a>| -> Result<PendingTest<'a>> {
        let dir = context_dir_name(&spec.display, &mut context_names);
        spawn_test(&spec, contexts_dir.join(dir), config, &env)
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

        // Whoever was reading our output has gone away (`attest | head -1`, or
        // quitting out of a pager). The tests still running have nobody left to
        // report to, so wind the run down like an interrupt rather than running
        // it out in silence; `main` then exits on a real SIGPIPE.
        if output::output_closed() {
            pending_list.clear(); // drop kills the trees and removes contexts
            status.finish();
            return Ok(results);
        }

        // Stream xtrace output from the current holder.
        if let Some(t) = tailed.as_mut()
            && t.has_new()
        {
            status.suspend(|| t.flush_new(false));
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
        let mut completed: Vec<TestResult<'a>> = Vec::new();
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
            let pending = pending_list.remove(i);
            if bail_flag {
                if config.verbose >= 2 {
                    let context = pending.context.as_deref().unwrap();
                    status.suspend(|| finish_xtrace(&mut tailed, &pending.name, context));
                }
                continue; // Drop kills + cleans up
            }
            let result = build_result(pending, exit_status, config, &env);
            if let Some(ref save_dir) = config.save_context {
                save_test_context(&result, save_dir, &env.submounts);
            }
            completed.push(result);
        }

        // Print results and start new tests.
        // Sort completed by name for deterministic output within a reap batch,
        // except that the test holding the xtrace tail goes first: its header
        // and the start of its trace are already on the terminal, so the rest
        // of it has to follow them before any other test's trace is dumped.
        let holder = tailed.as_ref().map(|t| t.name.as_str());
        completed.sort_by(|a, b| {
            (Some(a.name.as_str()) != holder, &a.name)
                .cmp(&(Some(b.name.as_str()) != holder, &b.name))
        });
        for result in completed {
            if config.verbose >= 2 {
                status.suspend(|| finish_xtrace(&mut tailed, &result.name, &result.context));
            }
            if config.json {
                output::print_test_result_json(&result);
            } else {
                status.record(result.outcome.passed());
                if config.verbose >= 1 || !result.outcome.passed() {
                    status.suspend(|| output::print_test_result(&result));
                }
                if !result.outcome.passed() {
                    if result.commands_succeeded() {
                        // Every command ran and returned zero; the trace has
                        // no failure in it to replay, and the last command
                        // is not to blame. The condition is.
                        status.suspend(|| crate::diagnostics::print_violations(&result));
                    } else {
                        // At -vv the streamer already showed this test's xtrace.
                        if config.verbose < 2 {
                            status.suspend(|| dump_xtrace_log(&result.name, &result.context));
                        }
                        status.suspend(|| crate::diagnostics::print_failure_snippet(&result));
                    }
                }
            }
            if !result.outcome.passed() && config.bail {
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

        // If the xtrace tail is free, hand it to the next pending test.
        if config.verbose >= 2
            && tailed.is_none()
            && let Some(p) = pending_list.first()
        {
            status.suspend(|| tailed = Tailed::open(p));
        }

        // Update status line with currently running tests.
        let running: Vec<(&str, Duration)> = pending_list
            .iter()
            .map(|p| {
                let duration = p
                    .cgroup
                    .as_ref()
                    .and_then(|cg| cg.read_cpu_time())
                    .unwrap_or_else(|| p.start.elapsed());
                (p.name.as_str(), duration)
            })
            .collect();
        status.update(&running);

        let mut listeners: Vec<&mut seccomp::Listener> = pending_list
            .iter_mut()
            .filter_map(|p| p.listener.as_mut())
            .collect();
        waiter.wait(POLL_INTERVAL, &mut listeners);
    }

    status.finish();

    if !config.json {
        output::print_summary(&results, wall_start.elapsed());
    }

    Ok(results)
}

/// Create the temporary directory that holds every test context of one run,
/// readable and writable by its owner alone.
///
/// A context dir is the whole record of what a test did: `stdout.log` and
/// `xtrace.log`, the test's working directory, any `--override` binaries,
/// and the overlay upper layers — which is every
/// file the test wrote anywhere in its ephemeral root. The runner enables
/// `set -x`, so `xtrace.log` holds each command with its arguments already
/// expanded: a test that passes a token, a password or a key to anything has
/// it written out verbatim.
///
/// None of that may be left for the rest of the machine to read. `tempfile`
/// makes temporary *files* private but creates temporary *directories* with
/// the plain umask (its own documentation calls them "world-readable by
/// default"), and the default parent is the shared `/tmp` — so by default
/// every local user could read all of the above, including when attest runs
/// under `sudo` (which is how privileged overlay isolation is usually
/// obtained). The per-test dirs underneath need no mode of their own: a
/// private parent cannot be traversed, so nothing inside it can be reached.
///
/// This narrows the mode, and can only ever narrow it: `mkdir(2)` still
/// applies the umask, so a stricter umask stays in force.
fn private_temp_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
}

/// Derive the name of a test's context directory from its display name,
/// keeping it a single path component that is unique within the run.
///
/// Test names come from shell function definitions and shells accept almost
/// anything there, `/` and `..` included (`test_x/../../../etc() { … }` parses
/// and runs fine under sh and bash). Joining such a name onto the run's temp
/// dir escapes it, and the escape is not contained by test isolation: the
/// parent process creates the context dir, opens the test's logs in it, and
/// `remove_dir_all`s it on drop, all outside any overlay and
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
    claim_unique(base, '_', taken)
}

/// `base`, or `base<sep>2`, `base<sep>3`, … — the first not yet in `taken`,
/// which it is added to.
pub(crate) fn claim_unique(
    base: String,
    sep: char,
    taken: &mut std::collections::HashSet<String>,
) -> String {
    if taken.insert(base.clone()) {
        return base;
    }
    (2..)
        .map(|k| format!("{base}{sep}{k}"))
        .find(|candidate| taken.insert(candidate.clone()))
        .expect("an unbounded range")
}

/// Spawn a child process that will run the test. Returns a `PendingTest` that
/// the caller must reap (or simply drop to kill+clean up). `context` must be
/// unique per test (the caller derives it from the unique display name).
fn spawn_test<'a>(
    spec: &TestSpec<'a>,
    context: PathBuf,
    config: &RunConfig,
    env: &RunEnv,
) -> Result<PendingTest<'a>> {
    let display_name = &spec.display;
    std::fs::create_dir_all(&context)?;

    // Fresh, empty working directory for the test. It lives in the context dir
    // (live-bound inside the ephemeral root), so it is picked up by
    // --save-context without being part of any delta.
    std::fs::create_dir_all(cwd_dir(&context))?;

    // Plan the per-test ephemeral root (this also creates the overlay dirs
    // under the context dir).
    let root_plan = overlay::RootOverlay::build(
        env.overlay_mode,
        &context,
        &env.invocation_dir,
        &env.submounts,
    )?;

    // The test's output goes to log files in its context dir, opened here and
    // handed to the child as its stdout and stderr instead of being redirected
    // by the script it runs. The script carries the file's own function
    // definitions, so a shell that will not parse them never reaches its first
    // line — and that complaint is exactly what the failure report has to
    // replay, so it belongs in `xtrace.log` with everything else the test
    // wrote to stderr.
    let stdout_log = std::fs::File::create(context.join("stdout.log"))?;
    let xtrace_log = std::fs::File::create(context.join("xtrace.log"))?;

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

    // Settled once per file at parse time, so a spawn neither re-reads the
    // script's shebang nor looks the shell up on PATH again.
    let shell = &spec.file.shell;

    // Where no cgroup can be had the test runs without one: resource stats
    // are absent from its result, and only a decorator that asks for them
    // says so (`warn_unmeasured`).
    let cgroup = crate::cgroup::TestCgroup::try_create(display_name);

    // The functions are rendered once per file at parse time (see
    // `parser::build_functions_source`); the script around them is per test.
    let runner_content = build_runner_script(
        spec.name,
        &spec.file.script,
        &context,
        &config.bin_dirs,
        &config.strace,
    );
    // <shell> -c <script> <path>: the operand after the script is $0, so
    // inside the test functions it names the original file.
    let source = spec.file.canonical_path.to_str().unwrap_or("bash");
    let mut spawn = Spawn::new(
        shell,
        &[shell, "-c", &runner_content, source],
        stdout_log,
        xtrace_log,
    )?;
    // bash fills in an unset `SHELL` from the passwd database at startup, and
    // glibc asks nscd first — a `socket` and a `connect` that the test never
    // made, in the very syscall log a `syscalls not in {socket}` condition is
    // judged on. Whether that happens would then depend on whether whoever
    // ran attest exported `SHELL` (a container or a CI job usually does not),
    // so a test would pass on one machine and fail on the next. Naming the
    // shell the test runs under keeps the lookup out of every test.
    if std::env::var_os("SHELL").is_none() {
        spawn.env("SHELL", shell);
    }

    // Detach into a fresh session/process group so timeouts and cleanup can
    // kill the whole test tree with one kill(-pgid).
    //
    // The signal mask is emptied in the same hook: the run blocks SIGCHLD in
    // the parent (see `ChildWaiter`) and a mask survives both fork and exec, so
    // a test's shell would otherwise start with SIGCHLD blocked — and a shell
    // that cannot take SIGCHLD does not notice its own background jobs
    // finishing, which is `wait` in a test hanging. Nothing here allocates,
    // as the hook requires.
    spawn.hook(|| {
        unsafe {
            libc::setsid();
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        }
        Ok(())
    });

    // Place the child into its cgroup after fork, before exec. The parent has
    // live threads (status-bar ticker), so the hook must not allocate:
    // add_self_to is async-signal-safe and the path CString is built here.
    // TODO don't include test setup in cgroup
    if let Some(ref cg) = cgroup
        && let Some(procs) = cg.procs_cstring()
    {
        spawn.hook(move || {
            unsafe { crate::cgroup::add_self_to(&procs) };
            Ok(())
        });
    }

    // Registered after the cgroup hook so cgroup placement happens in the host
    // namespace before we unshare into a private mount namespace.
    overlay::register_root_mount(&mut spawn, root_plan);

    // Last of all, so that attest's own mounts are not in the count: the
    // seccomp filter for a test that asserts on its syscalls, where this
    // kernel can answer one. The child installs it and waits, without a
    // syscall, for this side to take the listener out of it (see
    // `seccomp::Handoff`); the spawn is then waited for with the listener in
    // hand, since the exec itself may be the first syscall notified.
    let watched: Vec<_> = spec
        .file
        .conditions_of(spec.name)
        .flat_map(|c| c.kind.watched_syscalls())
        .collect();
    let filter = env
        .seccomp
        .then(|| seccomp::Filter::watching(&watched))
        .flatten();
    let handoff = match filter {
        Some(filter) => {
            let handoff = seccomp::Handoff::new()?;
            let end = handoff.child_end();
            spawn.hook(move || unsafe { filter.install_and_hand_off(end) });
            Some(handoff)
        }
        None => None,
    };

    let start = Instant::now();
    let spawning = spawn.start()?;
    let mut listener = match &handoff {
        Some(handoff) => match handoff.take(spawning.pid()) {
            Ok(listener) => listener,
            Err(e) => {
                // `take` killed the child, which waited on it; reap it.
                if let Ok(mut child) = spawning.wait_exec(None) {
                    let _ = child.wait();
                }
                return Err(anyhow!(
                    "spawn failed for {display_name}: taking the seccomp listener: {e}"
                ));
            }
        },
        None => None,
    };
    let child = spawning.wait_exec(listener.as_mut()).map_err(|e| {
        // The script rides in the argument vector, and Linux caps how long a
        // single argument may be — 32 pages classically, a slice of the stack
        // limit on newer kernels, so half a megabyte of shell either way. Only
        // a file with more function definitions in it than that can hit this,
        // and `E2BIG` on its own names neither the argument nor its size.
        let note = if e.raw_os_error() == Some(libc::E2BIG) {
            format!(
                " (the script carrying this file's function definitions is {} bytes, \
                     more than one argument can hold)",
                runner_content.len()
            )
        } else {
            // The probe passed, so this is a per-test setup failure: the
            // errno is all that survives the hook, and it belongs to one
            // of the mounts the ephemeral root is built from — or to the
            // seccomp filter installed after them.
            " (while building the test's ephemeral root)".to_string()
        };
        anyhow!("spawn failed for {display_name}: {e}{note}")
    })?;

    Ok(PendingTest {
        child,
        timed_out: false,
        name: display_name.clone(),
        start,
        context: Some(context),
        file: spec.file,
        test: spec.name,
        cgroup,
        listener,
    })
}

/// Build a `TestResult` from a `PendingTest` whose child has already exited
/// with the given status.
fn build_result<'a>(
    mut pending: PendingTest<'a>,
    status: ExitStatus,
    config: &RunConfig,
    env: &RunEnv,
) -> TestResult<'a> {
    let duration = pending.start.elapsed();
    let mut outcome = if pending.timed_out {
        Outcome::Timeout
    } else if status.success() {
        Outcome::Pass
    } else {
        Outcome::Fail
    };

    // Read stats before dropping cgroup (which removes the directory).
    let resources = pending.cgroup.as_ref().map(|cg| cg.read_stats());

    // Whatever was heard, pending answers included: the test is over.
    let syscalls = pending.listener.take().map(|l| l.into_log());

    // A decorator is a condition on a run of the test, and a run that failed
    // on its own — or was killed on the clock — is not one it says anything
    // about: the numbers are those of a test that never got to the end.
    let mut violations = Vec::new();
    if outcome == Outcome::Pass {
        let conditions: Vec<&decorator::Condition> =
            pending.file.conditions_of(pending.test).collect();
        // The delta is only walked for a test that asserts on it: it is as
        // big as the test made it, and almost no test says anything about it.
        let writes = if conditions
            .iter()
            .any(|c| matches!(c.kind, decorator::Kind::Writes(_)))
        {
            let context = pending.context.as_deref().unwrap();
            match overlay::delta_paths(context, &env.submounts) {
                Ok(paths) => Some(paths),
                Err(e) => {
                    debug!(test = %pending.name, "could not read the delta: {e}");
                    None
                }
            }
        } else {
            None
        };
        let sample = decorator::Sample {
            wall: duration,
            resources: resources.as_ref(),
            syscalls: syscalls.as_ref(),
            writes: writes.as_deref(),
            project: &env.invocation_dir,
        };
        for condition in conditions {
            match condition.check(&sample) {
                decorator::Verdict::Ok => {}
                decorator::Verdict::Violated(v) => violations.push(v),
                decorator::Verdict::Unmeasured(measure) => warn_unmeasured(measure),
            }
        }
        // A run that exists to find the right bounds is not failed by the
        // wrong ones; the violations are what it came for.
        if !violations.is_empty() && !config.update_decorators {
            outcome = Outcome::Fail;
        }
    }

    TestResult {
        name: pending.name.clone(),
        test: pending.test,
        file: pending.file,
        outcome,
        duration,
        context: pending.context.take().unwrap(),
        resources,
        syscalls,
        violations,
    }
    // pending drops here: a reaped child skips kill/wait, context=None skips
    // dir removal, cgroup drops removing the cgroup directory.
}

/// Say once per run, per metric, that conditions on it are going unchecked.
///
/// On stderr directly rather than in the log: a condition that is written
/// down and not checked changes what the run covers, so it has to show up
/// whatever the log level is — but once, since every test's `cpu` condition
/// is unmeasured for the same reason.
fn warn_unmeasured(measure: decorator::Measure) {
    use decorator::{Measure, Metric};
    static WARNED: std::sync::Mutex<Vec<Measure>> = std::sync::Mutex::new(Vec::new());
    {
        let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
        if warned.contains(&measure) {
            return;
        }
        warned.push(measure);
    }
    let hint = match measure {
        Measure::Metric(Metric::Wall) | Measure::Writes => "",
        Measure::Metric(_) => {
            " (cgroup v2 unavailable or the controller is not enabled; -d says which)"
        }
        Measure::Syscalls => " (seccomp user notification needs Linux 5.6 or later; -d says why)",
    };
    errln!(
        "warning: `{}` conditions are not checked: {} did not report it{hint}",
        measure.name(),
        measure.source()
    );
}

/// Is `dir` a directory of its own (not a symlink to one) holding at least one
/// entry?
fn dir_non_empty(dir: &Path) -> bool {
    overlay::is_real_dir(dir) && std::fs::read_dir(dir).is_ok_and(|mut e| e.next().is_some())
}

/// Every directory `--save-context` reads out of a finished test's context, as
/// `(what it is, where it is now, where it goes under the save dir)`.
///
/// Shallowest-first and in the order they have to be copied: the root
/// overlay's upper layer lands at the top of the saved tree, each ephemeral
/// submount's goes at the absolute path it stands for *inside* that tree, and
/// the two directories the runner writes into the context dir itself take
/// names of their own. The working directory is bind-mounted live, so it is in
/// no overlay and no delta; the `--strace` logs never go through one either.
///
/// Listed rather than copied here so that one loop in [`save_test_context`]
/// does all four, under one rule for what counts as something to copy, one
/// for how a destination is created, and one for how a failure is reported.
/// Written out a directory at a time, those were four near-copies that had
/// already come out differently: the root layer was copied whenever it was a
/// directory while the rest wanted a non-empty one, and only a submount delta
/// had its destination built with [`overlay::create_dir_nofollow`] (the others
/// go one component deep, where `copy_dir_recursive`'s own `unlink_if_symlink`
/// happens to cover it).
fn saved_dirs(
    result: &TestResult,
    submounts: &[overlay::Submount],
) -> Vec<(String, PathBuf, PathBuf)> {
    let context = &result.context;
    let mut dirs: Vec<(String, PathBuf, PathBuf)> = overlay::delta_dirs(context, submounts)
        .into_iter()
        .map(|(upper, at)| {
            let what = if at == Path::new("/") {
                "the filesystem delta".to_string()
            } else {
                format!("the {} delta", at.display())
            };
            let rel = at.strip_prefix("/").unwrap_or(&at).to_path_buf();
            (what, upper, rel)
        })
        .collect();
    dirs.push((
        "the working directory".to_string(),
        cwd_dir(context),
        PathBuf::from("cwd"),
    ));
    dirs.push((
        "the strace logs".to_string(),
        context.join("strace"),
        PathBuf::from("strace"),
    ));
    dirs
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
    // Created up front rather than as a side effect of the first copy: a test
    // that wrote nothing has no delta to copy at all, and the logs below
    // still have to land somewhere.
    if let Err(e) = std::fs::create_dir_all(&dst) {
        warn!("failed to save context for {}: {e}", result.name);
        return;
    }

    for (what, src, rel) in saved_dirs(result, submounts) {
        if !dir_non_empty(&src) {
            continue;
        }
        // Every destination is laid out inside the tree the entries before it
        // have already filled, so any of its leading components may be a
        // symlink the test planted (`/var` replaced by a link to the user's
        // home, say): create them explicitly instead of letting
        // `create_dir_all` follow one.
        let copied = overlay::create_dir_nofollow(&dst, &rel)
            .and_then(|into| overlay::copy_dir_recursive(&src, &into));
        if let Err(e) = copied {
            warn!("failed to save {what} for {}: {e}", result.name);
        }
    }

    for log in ["stdout.log", "xtrace.log"] {
        // Both ends of this copy are names a test gets to choose. The source
        // sits in its live context dir, where it can swap the log for a link
        // to a host file and have it copied out as its own trace; the
        // destination may already hold a symlink reproduced from the delta (a
        // test that created `/stdout.log` as one), and writing the log through
        // that would land on whatever host path it names.
        let Ok(mut from) = overlay::open_nofollow(&result.context.join(log)) else {
            continue;
        };
        let to = dst.join(log);
        let _ = overlay::unlink_if_symlink(&to);
        if let Ok(mut out) = std::fs::File::create(&to) {
            let _ = std::io::copy(&mut from, &mut out);
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

/// Build the shell script content that defines the test's functions and runs
/// the named one. Used with `<shell> -c <content> <path>` so that `$0`
/// inside test functions refers to the original script.
///
/// The definitions are carried in the script itself rather than sourced out of
/// a file beside it. `.` is the one command here that a shell need not have —
/// it is a POSIX special builtin, but attest runs whatever shell a file's
/// shebang names, and a small or purpose-built one may well implement neither
/// it nor `source` nor `eval` (issue #93). Inlining asks the shell for nothing
/// beyond defining a function and calling it, which is the whole of what a
/// test is.
///
/// Nothing is emitted in front of the definitions, and that is load-bearing:
/// they are rendered at the line numbers they have in the test file
/// (`parser::build_functions_source` pads for it) and `$LINENO` inside a
/// function is a line of the script the shell is running, so every `PS4` trace
/// line indexes the original source directly — which is how `diagnostics`
/// points at the command that failed. A prelude above them would shift all of
/// it by its own length.
fn build_runner_script(
    test_name: &str,
    functions: &str,
    working_dir: &Path,
    bin_dirs: &[PathBuf],
    strace: &[String],
) -> String {
    use std::fmt::Write;

    let mut s = String::from(functions);
    // `build_functions_source` ends every function on a line of its own, but a
    // file holding none at all renders as the empty string.
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }

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

    // Nothing redirects the logs here: the child is handed them as its stdout
    // and stderr before it is exec'd (see `spawn_test`), so a shell that
    // rejects the definitions above still complains into `xtrace.log` rather
    // than over attest's own output.
    s.push_str("set -e\n");

    // Every test starts in its own clean, empty working directory.
    let _ = writeln!(s, "cd {}", sh_quote(&cwd_dir(working_dir)));

    // Only now turn on xtrace, so the log starts at the test itself rather
    // than at the definitions and the setup above.
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

/// One `--fuzz` tick for the test rooted at `test_pid`: either pause a
/// running descendant or resume a stopped one, never both. `level` is the
/// aggressiveness in (0,1) — the higher it is, the more often a tick pauses
/// rather than resumes.
///
/// The descendants are walked from a random offset and the first one in the
/// state this tick wants is signalled. Whether a process is in that state is
/// settled by [`signal_tree_member`] alone, through the same pinned descriptor
/// it then signals through: a state read off `/proc/<pid>/stat` beforehand can
/// only ever be advisory, since the process is free to run, stop or exit
/// between that read and the signal. So shortlisting the eligible pids first —
/// a `/proc` read per descendant per tick, for every running test — told us
/// nothing the pinned check does not establish for itself, and wasted the tick
/// outright whenever the one pid drawn from the shortlist had moved on in the
/// meantime. Walking on to the next candidate instead means a tick acts
/// whenever anything in the tree is still eligible by the time it is asked.
fn fuzz_tick(test_pid: u32, level: f64, rng: &mut u64) {
    let resume = (xorshift64(rng) as f64) / (u64::MAX as f64) >= level;
    let (signal, verb) = if resume {
        (libc::SIGCONT, "Resumed")
    } else {
        (libc::SIGSTOP, "Paused")
    };
    let tree = collect_descendants(test_pid);
    // Everything but the test's own shell, which is `tree`'s first element.
    // Stopping the process the runner is waiting on would suspend the test
    // wholesale rather than reorder anything inside it.
    let descendants = tree.get(1..).unwrap_or_default();
    if descendants.is_empty() {
        return;
    }
    let offset = (xorshift64(rng) as usize) % descendants.len();
    for i in 0..descendants.len() {
        let pid = descendants[(offset + i) % descendants.len()];
        if signal_tree_member(pid, &tree, resume, signal) {
            trace!(pid, "{verb} subprocess");
            return;
        }
    }
}

/// Send `signal` to `pid`, but only if it is still the member of `tree` the
/// caller took it for and it is stopped exactly when `want_stopped` says.
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
/// That makes this the only place the state is worth reading at all — see
/// [`fuzz_tick`].
fn signal_tree_member(pid: u32, tree: &[u32], want_stopped: bool, signal: libc::c_int) -> bool {
    let Some(target) = PinnedProcess::open(pid) else {
        return false;
    };
    // Read through the pinned descriptor, so the process described here is
    // provably the one signalled below.
    let Some(stat) = target.stat() else {
        return false; // already gone
    };
    // A descendant whose state cannot be made out counts as running.
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
/// `/proc/<pid>/task/<pid>/children` recursively. `root` is the first element,
/// so the rest of the slice is exactly its descendants (see [`fuzz_tick`]).
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
    use std::process::Command;
    use tempfile::TempDir;

    impl Tailed {
        /// [`Tailed::flush_new`] into `out` rather than stderr (see
        /// [`Tailed::flush_new_via`]).
        fn flush_into(&mut self, out: &mut Vec<u8>, last: bool) {
            self.flush_new_via(last, |log, last| output::replay_log_to(out, log, last));
        }
    }

    /// How overlays can be mounted where the tests are running, probed once
    /// per process — or `None` where they cannot be at all, in which case a
    /// test that spawns anything returns early: a test is only ever run in an
    /// overlay, so there is nothing else to run it in.
    fn overlay_mode() -> Option<overlay::Mode> {
        static MODE: std::sync::OnceLock<Option<overlay::Mode>> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| {
            let scratch = TempDir::new().unwrap();
            let here = std::env::current_dir().unwrap();
            let submounts = overlay::compute_submounts(&here);
            match overlay::probe_support(scratch.path(), &here, &submounts) {
                Ok(mode) => Some(mode),
                Err(e) => {
                    eprintln!("skipping: {e}");
                    None
                }
            }
        })
    }

    /// A RunEnv with real isolation, invoked from `invocation_dir`, or `None`
    /// where this environment cannot mount overlays (see [`overlay_mode`]).
    fn overlay_env(invocation_dir: &Path) -> Option<RunEnv> {
        Some(RunEnv {
            invocation_dir: invocation_dir.to_path_buf(),
            overlay_mode: overlay_mode()?,
            submounts: overlay::compute_submounts(invocation_dir),
            seccomp: seccomp::probe().is_ok(),
        })
    }

    /// Write `script` to `dir/t.sh` and parse it. The result carries the path it
    /// was parsed from, so callers that need it read `tf.path`.
    fn parse_script(dir: &Path, script: &str) -> crate::parser::TestFile {
        let path = dir.join("t.sh");
        fs::write(&path, script).unwrap();
        crate::parser::parse_test_file(&path, &crate::parser::Prescan::default()).unwrap()
    }

    /// Block until `pending` finishes, reaping it exactly the way the poll
    /// loop in `run_all_tests` does so tests exercise that path — answering
    /// its seccomp listener meanwhile, as that loop's `ChildWaiter::wait`
    /// does: a test with a `syscalls` condition is stopped in its first
    /// watched syscall until the notification is answered, so without this
    /// it never exits.
    fn reap_blocking(pending: &mut PendingTest) -> ExitStatus {
        loop {
            if let Some(listener) = pending.listener.as_mut() {
                listener.service();
            }
            if let Some(status) = pending.reap().expect("reap failed") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// One test of `tf`, reported under `display`.
    fn spec<'a>(tf: &'a crate::parser::TestFile, name: &'a str, display: &str) -> TestSpec<'a> {
        TestSpec {
            display: display.to_string(),
            name,
            file: tf,
        }
    }

    /// A finished test together with the temp dir its context lives in.
    ///
    /// `build_result` moves the context path out of the `PendingTest`, so from
    /// then on nothing removes that directory any more — in a real run the
    /// run's own temp dir is what finally takes it away. A unit test has no
    /// run, so it has to hold the guard for as long as it inspects the result:
    /// without one, every `cargo test` leaves one context per test behind in
    /// `/tmp` for good, each holding that test's script, logs, working
    /// directory and overlay delta.
    ///
    /// Derefs to the result, so a caller reads `.outcome`/`.context` as before.
    struct Finished {
        result: TestResult<'static>,
        _context: TempDir,
    }

    impl std::ops::Deref for Finished {
        type Target = TestResult<'static>;

        fn deref(&self) -> &TestResult<'static> {
            &self.result
        }
    }

    /// Spawn `test_name` out of `script` (written under `srcdir`, which is
    /// also the invocation dir) in its own ephemeral root and block until it
    /// finishes. `None` where no overlay can be mounted.
    fn run_script(
        srcdir: &Path,
        script: &str,
        test_name: &str,
        config: &RunConfig,
    ) -> Option<Finished> {
        let env = overlay_env(srcdir)?;
        // Leaked so the result can borrow it past this frame, as a real run's
        // results borrow the selection's parses.
        let tf: &'static _ = Box::leak(Box::new(parse_script(srcdir, script)));
        let test_name: &'static str = Box::leak(test_name.into());
        let ctx = private_temp_dir().unwrap();
        let mut pending = spawn_test(
            &spec(tf, test_name, test_name),
            ctx.path().to_path_buf(),
            config,
            &env,
        )
        .unwrap();
        let status = reap_blocking(&mut pending);
        Some(Finished {
            result: build_result(pending, status, config, &env),
            _context: ctx,
        })
    }

    /// `run_all_tests` input running every test in `tf` under its own name.
    fn test_refs(tf: &crate::parser::TestFile) -> Vec<TestSpec<'_>> {
        tf.tests.iter().map(|name| spec(tf, name, name)).collect()
    }

    /// A finished test named `name` whose context dir is `context`. Neither
    /// the outcome nor the source path matters to the code that reads a
    /// context back out, which is what this stands in for.
    fn result_in(context: PathBuf, name: &str) -> TestResult<'_> {
        TestResult {
            name: name.to_string(),
            test: name,
            file: crate::parser::TestFile::empty(),
            outcome: Outcome::Fail,
            duration: Duration::from_millis(1),
            context,
            resources: None,
            syscalls: None,
            violations: Vec::new(),
        }
    }

    /// Run `test_name` from `script` with no extra configuration. `None`
    /// where no overlay can be mounted.
    fn run_inline(script: &str, test_name: &str) -> Option<Finished> {
        let tmp = TempDir::new().unwrap();
        run_script(tmp.path(), script, test_name, &RunConfig::default())
    }

    /// `wall` needs no cgroup, so it is the decorator a unit test can count
    /// on being checked wherever the tests run.
    #[test]
    fn a_decorator_the_run_breaks_fails_the_test() {
        let script = "#attest(wall < 1us)\ntest_x() {\n  true\n}\n";
        let Some(r) = run_inline(script, "test_x") else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Fail);
        assert_eq!(r.violations.len(), 1, "{:?}", r.violations);
        let v = &r.violations[0];
        assert_eq!(v.line, 1);
        assert_eq!(v.condition, "wall < 1us");
        assert!(v.message.starts_with("wall was "), "{}", v.message);
        // The commands themselves all ran: the report must not blame one.
        assert!(r.commands_succeeded());
    }

    #[test]
    fn a_decorator_the_run_meets_leaves_the_test_passing() {
        let script = "# attest(\n#   wall < 60s,\n# )\ntest_x() {\n  true\n}\n";
        let Some(r) = run_inline(script, "test_x") else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Pass);
        assert!(r.violations.is_empty());
    }

    #[test]
    fn under_update_decorators_a_violation_is_recorded_but_does_not_fail() {
        let script = "#attest(wall < 1us)\ntest_x() {\n  true\n}\n";
        let tmp = TempDir::new().unwrap();
        let config = RunConfig {
            update_decorators: true,
            ..Default::default()
        };
        let Some(r) = run_script(tmp.path(), script, "test_x", &config) else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Pass);
        assert_eq!(r.violations.len(), 1);
        assert_eq!(r.test, "test_x");
        // The literal to rewrite is where the file has it.
        let decorator::Detail::Quantity { broken, .. } = &r.violations[0].detail else {
            panic!("expected a quantity");
        };
        let lit = &broken.bound.literal;
        assert_eq!(lit.line, 1);
        assert_eq!(&script.lines().next().unwrap()[lit.span.clone()], "1us");
    }

    #[test]
    fn a_failing_test_is_not_also_judged_on_its_decorators() {
        // The numbers are those of a run that never got to the end, so the
        // only verdict is the command's — and the report stays the command's
        // too.
        let script = "#attest(wall < 1us)\ntest_x() {\n  false\n}\n";
        let Some(r) = run_inline(script, "test_x") else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(r.violations.is_empty());
        assert!(!r.commands_succeeded());
    }

    /// A `syscalls` condition is checked against what the seccomp listener
    /// heard from the whole tree, the shell included: the runner script's own
    /// `cd` is a `chdir` the shell makes, so forbidding it is a violation
    /// every run commits, and `mknodat` is one no shell makes.
    #[test]
    fn a_syscalls_condition_is_checked_against_what_was_heard() {
        if !seccomp::probe().is_ok() {
            eprintln!("skipping: seccomp user notification unavailable");
            return;
        }
        let Some(r) = run_inline(
            "#attest(syscalls not in {chdir, mknodat})\ntest_x() {\n  true\n}\n",
            "test_x",
        ) else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Fail);
        assert_eq!(r.violations.len(), 1, "{:?}", r.violations);
        assert_eq!(r.violations[0].message, "made chdir");
        let log = r.syscalls.as_ref().expect("the test was watched");
        assert!(
            log.counts
                .get(&decorator::Sysno::chdir)
                .is_some_and(|&n| n >= 1)
        );
        assert!(!log.counts.contains_key(&decorator::Sysno::mknodat));

        let Some(r) = run_inline(
            "#attest(syscalls not in {mknodat})\ntest_x() {\n  true\n}\n",
            "test_x",
        ) else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Pass, "{:?}", r.violations);
        assert_eq!(r.syscalls.as_ref().unwrap().counts.len(), 0);

        // The exec of the shell is itself a watched syscall here, and the
        // spawn must not wait for it in a way that stops it being answered.
        let Some(r) = run_inline(
            "#attest(syscalls in {read})\ntest_x() {\n  true\n}\n",
            "test_x",
        ) else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(
            r.violations[0].message.starts_with("also made "),
            "{}",
            r.violations[0].message
        );
        assert!(
            r.syscalls
                .as_ref()
                .unwrap()
                .counts
                .contains_key(&decorator::Sysno::execve)
        );
    }

    /// A test with no `syscalls` condition is not watched at all.
    #[test]
    fn a_test_without_a_syscalls_condition_is_not_watched() {
        let Some(r) = run_inline("#attest(wall < 60s)\ntest_x() {\n  true\n}\n", "test_x") else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Pass);
        assert!(r.syscalls.is_none());
    }

    /// A `writes` condition is checked against the delta: a write under an
    /// admitted root passes, one anywhere else names the path.
    #[test]
    fn a_writes_condition_is_checked_against_the_delta() {
        let Some(r) = run_inline(
            "#attest(writes in {/tmp})\ntest_x() {\n  echo x > /tmp/attest_writes_ok\n  echo y > ./in_cwd\n}\n",
            "test_x",
        ) else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Pass, "{:?}", r.violations);
        assert!(!Path::new("/tmp/attest_writes_ok").exists());

        let Some(r) = run_inline(
            "#attest(writes in {/tmp})\ntest_x() {\n  echo x > /attest_writes_marker\n  mkdir -p /var/attest_writes_dir/deep\n}\n",
            "test_x",
        ) else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Fail);
        assert_eq!(r.violations.len(), 1, "{:?}", r.violations);
        assert_eq!(
            r.violations[0].message,
            "wrote /attest_writes_marker, /var/attest_writes_dir"
        );
    }

    /// The run's temp dir carries every test's logs, working directory and
    /// filesystem delta, and `xtrace.log` carries each command with its
    /// arguments expanded — tokens and keys included. Its default parent is
    /// the shared `/tmp`, so nobody but its owner may be able to look inside.
    #[test]
    fn the_runs_temp_dir_is_private_to_its_owner() {
        let tmp = private_temp_dir().unwrap();
        let mode = fs::metadata(tmp.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "the run's temp dir is reachable by other users (mode {mode:04o})"
        );
        // Owner access is what the run itself needs, so narrowing must not
        // have taken that away either.
        assert_eq!(mode & 0o700, 0o700, "mode {mode:04o}");
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
        if overlay_mode().is_none() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let tf = parse_script(tmp.path(), "test_escape() {\n  echo hi > marker\n}\n");

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

        let results = run_all_tests(vec![spec(&tf, "test_escape", display)], &config).unwrap();
        assert!(results[0].outcome.passed());
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

    /// Is SIGCHLD blocked for the calling thread? Read by passing a null set to
    /// `pthread_sigmask`, which then only reports the current mask.
    fn sigchld_blocked() -> bool {
        let mut current: libc::sigset_t = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut current) };
        assert_eq!(ret, 0, "could not read the signal mask");
        let member = unsafe { libc::sigismember(&current, libc::SIGCHLD) };
        member == 1
    }

    #[test]
    fn child_waiter_blocks_sigchld_only_for_the_length_of_a_run() {
        let before = sigchld_blocked();
        {
            let waiter = ChildWaiter::new();
            assert!(waiter.sigfd.is_some(), "SIGCHLD could not be blocked");
            assert!(sigchld_blocked(), "SIGCHLD was not blocked for the run");
        }
        assert_eq!(
            sigchld_blocked(),
            before,
            "the run's signal mask outlived it"
        );
    }

    /// The wait has to end the moment a test exits rather than when its timer
    /// next comes round: that interval used to be a floor under every test (see
    /// [`ChildWaiter`]). `raise` leaves the signal pending on this thread, which
    /// is what an exiting child leaves pending on the run's.
    #[test]
    fn child_waiter_returns_at_once_for_a_child_that_has_already_signalled() {
        let waiter = ChildWaiter::new();
        assert!(waiter.sigfd.is_some(), "SIGCHLD could not be blocked");
        unsafe { libc::raise(libc::SIGCHLD) };
        let start = Instant::now();
        waiter.wait(Duration::from_secs(30), &mut []);
        let waited = start.elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "waited {waited:?} for a child that had already signalled"
        );
    }

    /// Whatever the parent does to its own mask, a test gets a clean one: masks
    /// survive fork and exec, and a shell that cannot take SIGCHLD does not see
    /// its own background jobs finish.
    #[test]
    fn a_test_does_not_inherit_the_runs_blocked_sigchld() {
        let _waiter = ChildWaiter::new();
        assert!(sigchld_blocked(), "SIGCHLD was not blocked for the run");
        let Some(result) = run_inline(
            r#"test_mask() {
  blocked=
  while read -r key value rest; do
    if [ "$key" = SigBlk: ]; then blocked=$value; fi
  done < /proc/self/status
  [ "$blocked" = 0000000000000000 ]
}
"#,
            "test_mask",
        ) else {
            return;
        };
        assert!(
            result.outcome.passed(),
            "the test ran with a signal blocked"
        );
    }

    /// A stand-in shell under `dir`, named so that discovery recognizes it,
    /// which announces on stdout that it was the one exec'd (`<dir name>
    /// ran`) and then exits successfully. Said rather than written to a
    /// marker file: the test runs in an ephemeral root, where a file the
    /// shim wrote would be discarded with the rest of the delta, while its
    /// stdout lands in the context's live-bound `stdout.log`.
    fn shell_shim(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let shell = dir.join("bash");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\necho {} ran\n",
                dir.file_name().unwrap().to_str().unwrap()
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        shell
    }

    /// A script defining `test_x` and asking to be run under `shell`.
    fn script_with_shebang(shell: &Path) -> String {
        format!("#!{}\ntest_x() {{\n  :\n}}\n", shell.display())
    }

    /// A spawn execs the shell its file was *parsed* with, rather than reading
    /// the file's shebang again when the test starts.
    ///
    /// The shebang used to be re-read off disk on every spawn while the script
    /// being run came from the parse, so the two could disagree: a file
    /// edited mid-run had its new shell paired with its old functions, and
    /// under `--repeat` that window reopened for every pass over the same
    /// `TestFile`.
    #[test]
    fn a_spawn_execs_the_shell_its_file_was_parsed_with() {
        let tmp = TempDir::new().unwrap();
        let Some(env) = overlay_env(tmp.path()) else {
            return;
        };
        let parsed = shell_shim(&tmp.path().join("parsed"));
        let edited = shell_shim(&tmp.path().join("edited"));

        let tf = parse_script(tmp.path(), &script_with_shebang(&parsed));
        assert_eq!(Path::new(&tf.shell), parsed);

        // The file changes its mind about the shell after the parse.
        fs::write(tf.path.as_path(), script_with_shebang(&edited)).unwrap();

        let ctx = private_temp_dir().unwrap();
        let mut pending = spawn_test(
            &spec(&tf, "test_x", "test_x"),
            ctx.path().to_path_buf(),
            &RunConfig::default(),
            &env,
        )
        .unwrap();
        assert!(reap_blocking(&mut pending).success());

        let stdout = fs::read_to_string(ctx.path().join("stdout.log")).unwrap();
        assert_eq!(
            stdout, "parsed ran\n",
            "the spawn did not exec the shell the file was parsed with"
        );
    }

    /// A stand-in for the shell issue #93 is about: one with no `.`, no
    /// `source` and no `eval`.
    ///
    /// It is bash with those three builtins taken away — `BASH_ENV` is sourced
    /// before a non-interactive bash runs its `-c` script, and `enable -n`
    /// takes a builtin away for the rest of the shell's life — so the shell
    /// really does lack them, rather than a script only claiming it does.
    fn no_dot_shell(dir: &Path) -> PathBuf {
        let env = dir.join("no_dot.sh");
        fs::write(&env, "enable -n . source eval\n").unwrap();
        let shell = dir.join("bash");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\nBASH_ENV={} exec {} \"$@\"\n",
                sh_quote(&env),
                sh_quote(&which::which("bash").unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        shell
    }

    /// A shell that implements neither `.` nor `source` nor `eval` still runs
    /// its tests: the function definitions are carried in the script the
    /// runner hands the shell, so running a test asks it for nothing beyond
    /// defining a function and calling it (issue #93).
    #[test]
    fn a_shell_without_a_dot_command_still_runs_its_tests() {
        let tmp = TempDir::new().unwrap();
        let shebang = format!("#!{}\n", no_dot_shell(tmp.path()).display());

        let Some(r) = run_script(
            tmp.path(),
            &format!(
                "{shebang}helper() {{\n  echo 42\n}}\ntest_x() {{\n  [ \"$(helper)\" = 42 ]\n}}\n"
            ),
            "test_x",
            &RunConfig::default(),
        ) else {
            return;
        };
        assert!(
            r.outcome.passed(),
            "a shell without `.` could not run the test: {}",
            fs::read_to_string(r.context.join("xtrace.log")).unwrap_or_default()
        );

        // And that shell really has no `.`, so the pass above is the runner
        // asking for no sourcing rather than `enable -n` quietly doing nothing.
        let sourcing = run_script(
            tmp.path(),
            &format!("{shebang}test_dot() {{\n  . /dev/null\n}}\n"),
            "test_dot",
            &RunConfig::default(),
        )
        .unwrap();
        assert!(
            !sourcing.outcome.passed(),
            "the stand-in shell still has a `.` command"
        );
    }

    /// Whatever the shell itself has to say lands in `xtrace.log`, where the
    /// failure report replays it from.
    ///
    /// The script the shell is given carries the test file's own function
    /// definitions, so a shell that will not parse them never reaches the
    /// first line of it. Nothing in the script can redirect the complaint that
    /// follows; the child is handed the logs as its stdout and stderr before
    /// it is exec'd, so it goes to the log either way instead of landing in
    /// the middle of attest's own output.
    #[test]
    fn the_shells_own_complaint_goes_to_the_xtrace_log() {
        let tmp = TempDir::new().unwrap();
        // A "shell" that rejects the script out of hand, the way a real one
        // would reject syntax it does not know.
        let shell = tmp.path().join("bash");
        fs::write(&shell, "#!/bin/sh\necho 'bad syntax' >&2\nexit 2\n").unwrap();
        fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();

        let Some(r) = run_script(
            tmp.path(),
            &format!("#!{}\ntest_x() {{\n  :\n}}\n", shell.display()),
            "test_x",
            &RunConfig::default(),
        ) else {
            return;
        };
        assert!(!r.outcome.passed(), "the shell exited 2");
        assert_eq!(
            fs::read_to_string(r.context.join("xtrace.log")).unwrap(),
            "bad syntax\n"
        );
    }

    /// Nothing may be emitted above the function definitions, and nothing the
    /// runner adds below them may be a command a shell can lack.
    ///
    /// The definitions are rendered at the line numbers they have in the test
    /// file and `$LINENO` counts from the first line of the script the shell is
    /// running, so a single line in front of them shifts every trace line a
    /// failure is mapped back to the source with.
    #[test]
    fn the_runner_script_starts_at_the_definitions() {
        let functions = "helper() {\n  :\n}\n\ntest_x() {\n  false\n}\n";
        let script = build_runner_script(
            "test_x",
            functions,
            Path::new("/ctx"),
            &[PathBuf::from("/bins")],
            &["strace_me".to_string()],
        );

        let Some(prelude) = script.strip_prefix(functions) else {
            panic!("the runner script does not start at the definitions:\n{script}");
        };
        for line in prelude.lines() {
            let word = line.split(|c: char| c.is_whitespace()).next().unwrap_or("");
            assert!(
                !matches!(word, "." | "source" | "eval"),
                "the runner script needs `{word}`: {line}"
            );
        }
    }

    #[test]
    fn execute_passing_test() {
        let Some(r) = run_inline("test_pass() {\n  true\n}\n", "test_pass") else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Pass);
    }

    #[test]
    fn execute_failing_test() {
        // A command that exits nonzero fails the test, and does so as a
        // failure rather than as a timeout.
        let Some(r) = run_inline("test_fail() {\n  false\n}\n", "test_fail") else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Fail);
    }

    #[test]
    fn execute_test_with_helper() {
        let Some(r) = run_inline(
            "get_value() {\n  echo 42\n}\ntest_helper() {\n  val=$(get_value)\n  test \"$val\" = \"42\"\n}\n",
            "test_helper",
        ) else {
            return;
        };
        assert_eq!(r.outcome, Outcome::Pass);
    }

    #[test]
    fn test_starts_in_clean_cwd() {
        let Some(r) = run_inline(
            "test_cwd() {\n  test -z \"$(ls -A .)\"\n  echo data > f.txt\n}\n",
            "test_cwd",
        ) else {
            return;
        };
        assert!(r.outcome.passed());
        assert!(cwd_dir(&r.context).join("f.txt").exists());
    }

    #[test]
    fn background_process_killed_after_normal_exit() {
        let Some(r) = run_inline(
            "test_bg() {\n  sleep 300 &\n  echo $! > pid\n}\n",
            "test_bg",
        ) else {
            return;
        };
        assert!(r.outcome.passed());
        let pid: u32 = fs::read_to_string(cwd_dir(&r.context).join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The straggler must be dead shortly after the test completes.
        assert!(await_death(pid, 5), "background process survived the test");
    }

    /// Read the single-character process state from `/proc/<pid>/stat`.
    /// `None` if the file cannot be read (e.g. the process has already
    /// exited). Only the assertions below look at a process this way: the
    /// runner itself reads state through a pinned descriptor, since the pid in
    /// a path may have been reissued (see [`signal_tree_member`]).
    fn process_state(pid: u32) -> Option<char> {
        stat_state(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
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
        let Some(env) = overlay_env(tmp.path()) else {
            return;
        };
        let tf = parse_script(
            tmp.path(),
            "test_bg() {\n  sleep 300 &\n  echo $! > pid\n}\n",
        );
        let ctx = private_temp_dir().unwrap();
        let config = RunConfig::default();
        let mut pending = spawn_test(
            &spec(&tf, "test_bg", "test_bg"),
            ctx.path().to_path_buf(),
            &config,
            &env,
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
        let Some(r) = run_inline("test_echo() {\n  echo captured_output\n}\n", "test_echo") else {
            return;
        };
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
        let Some(result) = run_script(
            tmp.path(),
            "test_override() {\n  true\n}\n",
            "test_override",
            &config,
        ) else {
            return;
        };
        assert!(result.outcome.passed());
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
        let Some(result) = run_script(
            tmp.path(),
            "test_bin_dir() {\n  out=$(mytool)\n  test \"$out\" = \"mytool_ran\"\n}\n",
            "test_bin_dir",
            &config,
        ) else {
            return;
        };
        assert!(result.outcome.passed());
        // The tool is referenced in place, not copied into the context bin/.
        assert!(!result.context.join("bin/mytool").exists());
    }

    #[test]
    fn overlay_isolates_root_writes() {
        // The test sees the real root (reads /usr) but its write to a system path
        // must land in the ephemeral upper layer, not on the host filesystem.
        let Some(result) = run_inline(
            "test_o() {\n  test -d /usr\n  echo made > /attest_root_marker.txt\n}\n",
            "test_o",
        ) else {
            return; // overlays unavailable here
        };
        assert!(result.outcome.passed());
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
        let Some(result) = run_inline(
            &format!("test_t() {{\n  echo made > /tmp/{marker}\n}}\n"),
            "test_t",
        ) else {
            return; // overlays unavailable here
        };
        assert!(result.outcome.passed());
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

        let result = result_in(ctx, "test_traced");

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
    fn save_context_never_reads_a_tests_logs_through_its_own_symlinks() {
        // A test's context dir is bind-mounted live into its ephemeral root
        // (the logs are written and the working directory lives there), so a
        // running test can unlink its own stdout.log/xtrace.log and leave a
        // symlink to any host path in its place. Reading the name afterwards
        // would copy that file out as the test's output — an arbitrary-file
        // read for anything the user running attest can open, landing in a
        // directory they are about to go and read.
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret");
        fs::write(&secret, "SECRET").unwrap();

        let contexts = TempDir::new().unwrap();
        let ctx = contexts.path().join("test_leak");
        fs::create_dir_all(&ctx).unwrap();
        for log in ["stdout.log", "xtrace.log"] {
            std::os::unix::fs::symlink(&secret, ctx.join(log)).unwrap();
        }
        // The same trick on the directories --save-context walks: a link here
        // would have the whole target tree copied out instead.
        std::os::unix::fs::symlink(outside.path(), overlay::upper_dir(&ctx)).unwrap();
        std::os::unix::fs::symlink(outside.path(), cwd_dir(&ctx)).unwrap();
        std::os::unix::fs::symlink(outside.path(), ctx.join("strace")).unwrap();

        let result = result_in(ctx, "test_leak");

        let save = TempDir::new().unwrap();
        save_test_context(&result, save.path(), &[]);

        // Nothing read through a link: no copy of the secret anywhere, under
        // any of the names it was linked from.
        let saved = save.path().join("test_leak");
        for name in ["stdout.log", "xtrace.log", "cwd/secret", "strace/secret"] {
            assert!(
                !saved.join(name).exists(),
                "{name} was read through the test's own symlink"
            );
        }
        assert!(!saved.join("secret").exists(), "upper layer was followed");
    }

    #[test]
    fn a_fifo_left_at_a_log_name_does_not_wedge_the_run() {
        // The same live context dir, with a sharper substitution than a
        // symlink: opening a FIFO read-only blocks until a writer shows up, and
        // `std` retries that call across `EINTR`. So a test ending in
        // `rm xtrace.log; mkfifo xtrace.log` used to leave attest stuck in
        // `open(2)` for good — after the FAIL line, with `^C` no help and the
        // context dirs it was still holding left behind in the temp dir.
        let contexts = TempDir::new().unwrap();
        let ctx = contexts.path().join("test_wedge");
        fs::create_dir_all(cwd_dir(&ctx)).unwrap();
        for log in ["stdout.log", "xtrace.log"] {
            use std::os::unix::ffi::OsStrExt;
            let path = std::ffi::CString::new(ctx.join(log).as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
        }

        let mut result = result_in(ctx, "test_wedge");
        let srcdir = TempDir::new().unwrap();
        let tf = parse_script(srcdir.path(), "test_wedge() {\n  false\n}\n");
        result.file = Box::leak(Box::new(tf));

        // Every place a finished test's logs are read back, on a deadline: a
        // regression is a hang rather than a failed assertion, so the test has
        // to be able to outlive it.
        let save = TempDir::new().unwrap();
        let save_dir = save.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            dump_xtrace_log(&result.name, result.context.as_path());
            crate::diagnostics::print_failure_snippet(&result);
            output::print_test_result_json(&result);
            save_test_context(&result, &save_dir, &[]);
            let _ = tx.send(());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(30)),
            Ok(()),
            "reading a test's logs back blocked on a FIFO it left behind"
        );

        // Nothing was invented for a log that is no longer a file either.
        assert!(!save.path().join("test_wedge/xtrace.log").exists());
        assert!(!save.path().join("test_wedge/stdout.log").exists());
    }

    #[test]
    fn xtrace_streaming_holds_back_a_c1_control_split_across_reads() {
        // `-vv` tails a running test's xtrace.log and sanitizes whatever each
        // read turned up. An encoded C1 control is two bytes, so a test that
        // writes them one at a time (`printf '\302'; printf '\233'`) can land
        // them in different reads — and `\xc2` alone then looks like ordinary
        // text to the sanitizer, as does `\x9b` alone. Nothing may be printed
        // until the byte after a trailing `\xc2` has been seen.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("xtrace.log");
        fs::write(&log, b"+1: payload\xc2").unwrap();

        let mut tailed = Tailed {
            name: "test_split".to_string(),
            file: overlay::open_nofollow(&log).unwrap(),
            offset: 0,
            interrupted: false,
        };

        let mut out = Vec::new();
        tailed.flush_into(&mut out, false);
        assert_eq!(
            tailed.offset, 11,
            "the trailing \\xc2 was printed before its second byte arrived"
        );
        assert_eq!(out, b"+1: payload");

        // The second half lands: the pair is now sanitized together.
        fs::write(&log, b"+1: payload\xc2\x9b2K").unwrap();
        tailed.flush_into(&mut out, false);
        assert_eq!(tailed.offset, 15);
        assert_eq!(out, br"+1: payload\xc2\x9b2K");

        // Once the test is done there is no more to wait for, so a log that
        // really does end in a lone `\xc2` is not held back forever.
        fs::write(&log, b"+1: payload\xc2\x9b2K\xc2").unwrap();
        tailed.flush_into(&mut out, true);
        assert_eq!(tailed.offset, 16);
        assert_eq!(out, b"+1: payload\\xc2\\x9b2K\xc2");
    }

    #[test]
    fn xtrace_streaming_reannounces_a_trace_another_test_cut_into() {
        // Under `-vv` a test that finishes while another holds the tail gets
        // its trace and result printed in the middle of the holder's trace.
        // The holder's next lines must then come under its header again, or
        // they read as the end of the test printed in between — but only once
        // it has something to say, so a quiet holder adds no empty header.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("xtrace.log");
        fs::write(&log, b"+1: first\n").unwrap();
        let other = TempDir::new().unwrap();

        let mut tailed = Some(Tailed {
            name: "test_holder".to_string(),
            file: overlay::open_nofollow(&log).unwrap(),
            offset: 0,
            interrupted: false,
        });
        let mut out = Vec::new();
        tailed.as_mut().unwrap().flush_into(&mut out, false);

        finish_xtrace(&mut tailed, "test_other", other.path());
        let holder = tailed.as_mut().expect("the holder keeps the tail");
        assert!(holder.interrupted, "another test's output went unnoticed");

        holder.flush_into(&mut out, false);
        assert!(holder.interrupted, "re-announced with nothing to follow");

        fs::write(&log, b"+1: first\n+2: second\n").unwrap();
        holder.flush_into(&mut out, false);
        assert!(!holder.interrupted);
        assert_eq!(holder.offset, 21);
        assert_eq!(out, b"+1: first\n+2: second\n");

        // The holder finishing releases the tail rather than dumping itself.
        finish_xtrace(&mut tailed, "test_holder", tmp.path());
        assert!(tailed.is_none());
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

        let result = result_in(ctx, "test_evil");
        let submounts = [overlay::Submount {
            source: PathBuf::from("/var/tmp"),
            is_dir: true,
            ephemeral_upper: Some(0),
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
        if overlay_mode().is_none() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let tf = parse_script(tmp.path(), "test_a() {\n  true\n}\ntest_b() {\n  true\n}\n");
        let config = RunConfig {
            parallel: 1,
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf), &config).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.outcome.passed()));
    }

    #[test]
    fn bail_stops_after_first_failure() {
        if overlay_mode().is_none() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        // test_fail comes first alphabetically, test_pass second
        let tf = parse_script(
            tmp.path(),
            "test_fail() {\n  false\n}\ntest_pass() {\n  true\n}\n",
        );
        let config = RunConfig {
            parallel: 1,
            bail: true,
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf), &config).unwrap();
        // Only the failing test ran; bail stopped execution
        assert_eq!(results.len(), 1);
        assert!(!results[0].outcome.passed());
    }

    #[test]
    fn run_all_tests_parallel() {
        if overlay_mode().is_none() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let tf = parse_script(
            tmp.path(),
            "test_x() {\n  true\n}\ntest_y() {\n  false\n}\n",
        );
        let config = RunConfig {
            parallel: 0,
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf), &config).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r.outcome.passed()));
        assert!(results.iter().any(|r| !r.outcome.passed()));
    }

    #[test]
    fn timeout_kills_slow_test() {
        if overlay_mode().is_none() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let tf = parse_script(tmp.path(), "test_slow() {\n  sleep 60\n}\n");
        let config = RunConfig {
            parallel: 1,
            timeout: Some(std::time::Duration::from_millis(200)),
            ..RunConfig::default()
        };

        let results = run_all_tests(test_refs(&tf), &config).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, Outcome::Timeout);
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

    /// A shell with `n` `sleep 30` children, and their pids. Each one is a
    /// direct child of the shell, so the `/proc` walk finds them all as
    /// descendants of the test.
    fn sleeper_tree(n: usize) -> (std::process::Child, Vec<u32>) {
        use std::io::BufRead;

        let spawn = "sleep 30 & echo $! >&2; ".repeat(n);
        let mut shell = Command::new("/bin/sh")
            .args(["-c", &format!("{spawn}wait")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(shell.stderr.take().unwrap());
        let pids: Vec<u32> = (0..n)
            .map(|_| {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let pid: u32 = line.trim().parse().unwrap();
                assert!(await_state(pid, 'S', 5), "sleeper {pid} never started");
                pid
            })
            .collect();
        (shell, pids)
    }

    #[test]
    fn fuzz_pauses_and_resumes_a_descendant() {
        // The baseline the hardening below must not break: a live descendant of
        // a test gets stopped by a pausing tick and resumed by a resuming one.
        let (mut shell, pids) = sleeper_tree(1);
        let sleeper = pids[0];

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
    fn fuzz_looks_past_a_descendant_in_the_wrong_state() {
        // A tick walks the descendants from a random offset and signals the
        // first one in the state it wants, so a descendant that is already
        // stopped must not absorb a pausing tick: whichever offset came up,
        // something that was running has to end up paused.
        let (mut shell, pids) = sleeper_tree(2);
        let (already_stopped, running) = (pids[0], pids[1]);
        unsafe { libc::kill(already_stopped as libc::pid_t, libc::SIGSTOP) };
        assert!(await_state(already_stopped, 'T', 5), "could not stop it");

        let mut rng = 1;
        fuzz_tick(shell.id(), 1.0, &mut rng); // level 1.0 always pauses
        let paused = await_state(running, 'T', 5);

        // Torn down before the verdict: a `sleep` left stopped by a failing
        // assertion would never exit on its own.
        for pid in pids {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = shell.kill();
        let _ = shell.wait();

        assert!(
            paused,
            "the tick gave up on the descendant that was already stopped"
        );
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
    fn xtrace_line_number_matches_source() {
        // The failing command sits on source line 7 (1-based). With the
        // definitions at the head of the runner script, at the line numbers
        // they have in the file, the xtrace PS4 must report `+7:`.
        let source = "helper() {\n  echo setup\n}\n\ntest_foo() {\n  echo a\n  false\n}\n";
        let Some(r) = run_inline(source, "test_foo") else {
            return;
        };
        assert!(!r.outcome.passed());

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

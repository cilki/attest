use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use serde_json::{Map, Value};

use crate::runner::TestResult;

pub(crate) const GREEN: &str = "\x1b[32m";
pub(crate) const RED: &str = "\x1b[31m";
pub(crate) const RESET: &str = "\x1b[0m";

/// Set once the reader on the other end of one of our streams has gone away, as
/// `attest | head -1` or quitting out of `attest | less` does.
///
/// Rust disables `SIGPIPE` for the whole process at startup, so writing to a
/// closed pipe does not kill us the way it kills an ordinary Unix filter:
/// instead the write fails with `EPIPE`, and that is an error `println!`
/// answers by panicking. A test runner panicking halfway through a run is a
/// bad look for a tool whose whole job is reporting failures, so every write
/// goes through [`write_stream`], which latches this flag instead. The run then
/// winds down the same way it does for ^C and `main` exits on a real `SIGPIPE`.
static BROKEN_PIPE: AtomicBool = AtomicBool::new(false);

/// Whether our output has nowhere left to go, so the run should wind down.
pub fn output_closed() -> bool {
    BROKEN_PIPE.load(Ordering::Relaxed)
}

/// Write `buf` to `stream`, latching `closed` when the reader is gone and
/// writing nothing at all once it is set.
///
/// Any other error is dropped: there is nowhere left to report a failure to
/// write a report, and a stream that is merely full should not abandon a run
/// that is otherwise fine.
fn write_stream(closed: &AtomicBool, stream: &mut impl Write, buf: &[u8]) {
    if closed.load(Ordering::Relaxed) {
        return;
    }
    if let Err(e) = stream.write_all(buf).and_then(|()| stream.flush())
        && e.kind() == std::io::ErrorKind::BrokenPipe
    {
        closed.store(true, Ordering::Relaxed);
    }
}

pub fn write_out(buf: &[u8]) {
    write_stream(&BROKEN_PIPE, &mut std::io::stdout().lock(), buf);
}

pub fn write_err(buf: &[u8]) {
    write_stream(&BROKEN_PIPE, &mut std::io::stderr().lock(), buf);
}

/// Write a captured log to stderr with the bytes that drive a terminal
/// escaped, so replaying it cannot rewrite the report around it.
///
/// A test's `xtrace.log` holds the shell's trace *and* everything the test and
/// the programs it ran wrote to stderr, so dumping it puts arbitrary bytes the
/// test chose on the terminal that is reporting on that test. Raw, those bytes
/// are commands: `ESC[2A ESC[2K` walks the cursor back over the `FAIL` line
/// and erases it, `\r` overwrites the line being written, `ESC[0m` escapes the
/// dim styling the dump is wrapped in, and OSC sequences reach the terminal's
/// title and (on a few terminals) its clipboard. A test that fails while
/// echoing content it fetched, generated or was handed as a fixture should not
/// be able to decide what the reader is told about it.
///
/// The full bytes are still reported verbatim where nothing interprets them:
/// `--json` and `--save-context`.
pub fn write_log_err(buf: &[u8]) {
    write_err(&sanitize_log(buf));
}

/// Whether `byte` has to be escaped before being replayed into a terminal:
/// every C0 control byte bar the `\n` and `\t` a trace legitimately contains,
/// plus `DEL`. Bytes >= `0x80` are left alone — a UTF-8 terminal reads them as
/// text rather than as C1 controls, and escaping them would mangle the logs of
/// every test that prints non-ASCII.
fn needs_escape(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0b..=0x1f | 0x7f)
}

/// Replace each byte [`needs_escape`] rejects with a `\xNN` spelling of it,
/// borrowing the input unchanged when there is nothing to escape (the usual
/// case, since a trace is mostly printable text).
///
/// Escaped byte-wise rather than over `char`s: a log is arbitrary bytes and
/// need not be valid UTF-8, and lossy decoding would turn a test's binary
/// output into a wall of U+FFFD. Backslashes are left as they are — bash
/// xtrace is full of them (`$'a\nb'`) and doubling every one would be a worse
/// trade than the ambiguity with a literal `\x1b` in the log text.
fn sanitize_log(buf: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !buf.iter().copied().any(needs_escape) {
        return std::borrow::Cow::Borrowed(buf);
    }
    let mut out = Vec::with_capacity(buf.len());
    for &byte in buf {
        if needs_escape(byte) {
            out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
        } else {
            out.push(byte);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// `println!` that winds the run down instead of panicking once stdout's reader
/// has gone away. See [`BROKEN_PIPE`].
macro_rules! outln {
    () => { $crate::output::write_out(b"\n") };
    ($($arg:tt)*) => {
        $crate::output::write_out(format!("{}\n", format_args!($($arg)*)).as_bytes())
    };
}

/// `eprintln!` counterpart of [`outln!`].
macro_rules! errln {
    () => { $crate::output::write_err(b"\n") };
    ($($arg:tt)*) => {
        $crate::output::write_err(format!("{}\n", format_args!($($arg)*)).as_bytes())
    };
}

pub(crate) use {errln, outln};

/// Number of blocks in the live progress strip. The whole run is scaled to fit
/// this width, so on a large suite a single block stands for several tests.
const BAR_WIDTH: usize = 32;

pub struct StatusDisplay {
    bar: Option<ProgressBar>,
    /// Total number of tests in the run, used to scale the block strip so it
    /// always spans the entire suite.
    total: usize,
    /// Pass/fail of each completed test, in completion order, used to draw the
    /// green/red block strip in the live status line.
    results: Vec<bool>,
}

impl StatusDisplay {
    pub fn new(total: usize, json: bool) -> Self {
        let visible = !json && !indicatif::ProgressDrawTarget::stderr().is_hidden();
        let bar = visible.then(|| {
            let bar = ProgressBar::new(total as u64);
            bar.set_style(
                ProgressStyle::default_bar()
                    .template("\x1b[1;32mTesting\x1b[0m {pos}/{len} {msg}")
                    .unwrap(),
            );
            bar.enable_steady_tick(Duration::from_millis(250));
            bar
        });
        Self {
            bar,
            total,
            results: Vec::new(),
        }
    }

    /// Record a completed test's outcome so the progress strip can show a
    /// green (pass) or red (fail) block for it.
    pub fn record(&mut self, passed: bool) {
        self.results.push(passed);
        if let Some(ref bar) = self.bar {
            bar.set_position(self.results.len() as u64);
        }
    }

    /// Render the run as a fixed-width strip of colored blocks that always
    /// spans the whole suite. The `total` tests are distributed across at most
    /// `BAR_WIDTH` blocks, so on a large run a single block stands for several
    /// tests: it is green once its tests have all passed, red as soon as any
    /// one of them fails, and an unfilled `░` until its tests start finishing.
    fn render_blocks(&self) -> String {
        if self.total == 0 {
            return String::new();
        }
        let width = self.total.min(BAR_WIDTH);
        let completed = self.results.len();
        let mut s = String::new();
        for b in 0..width {
            // Contiguous slice of the run this block represents.
            let lo = b * self.total / width;
            let hi = (b + 1) * self.total / width;
            let done = completed.min(hi).saturating_sub(lo);
            if done == 0 {
                // None of this block's tests have finished yet.
                s.push('░');
                continue;
            }
            let any_failed = self.results[lo..lo + done].iter().any(|&passed| !passed);
            s.push_str(if any_failed { RED } else { GREEN });
            s.push('█');
            s.push_str(RESET);
        }
        s
    }

    /// Update the status line with the result strip plus the currently running
    /// tests and their elapsed times.
    pub fn update(&self, running: &[(&str, Duration)], completed: usize) {
        if let Some(ref bar) = self.bar {
            bar.set_position(completed as u64);
            let running_msg: String = running
                .iter()
                .map(|(name, elapsed)| {
                    format!("{}({})", escape_label(name), format_duration(*elapsed))
                })
                .collect::<Vec<_>>()
                .join(", ");
            let blocks = self.render_blocks();
            let msg = match (blocks.is_empty(), running_msg.is_empty()) {
                (true, _) => running_msg,
                (false, true) => blocks,
                (false, false) => format!("{blocks}  {running_msg}"),
            };
            bar.set_message(msg);
        }
    }

    /// Run a closure with the status line temporarily hidden, so printed output
    /// doesn't collide with it.
    pub fn suspend<F: FnOnce()>(&self, f: F) {
        if let Some(ref bar) = self.bar {
            bar.suspend(f);
        } else {
            f();
        }
    }

    pub fn finish(&self) {
        if let Some(ref bar) = self.bar {
            bar.finish_and_clear();
        }
    }
}

/// Read one of a test's logs out of its context dir. Lossy so a log containing
/// invalid UTF-8 (e.g. binary output) is still reported rather than silently
/// becoming empty, and never through a symlink the test left at that name (see
/// [`crate::overlay::open_nofollow`]) — a JSON consumer would otherwise be fed
/// the contents of an arbitrary host file as the test's output.
fn read_log(path: PathBuf) -> String {
    let bytes = crate::overlay::read_nofollow(&path).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A test's `--strace` logs as `{"<cmd>": "<log>"}`, read from the
/// `strace/<cmd>.log` files the runner wrote into its context dir. Empty
/// without `--strace` (or when nothing was traced).
fn strace_logs(context: &Path) -> Map<String, Value> {
    let dir = context.join("strace");
    if !crate::overlay::is_real_dir(&dir) {
        return Map::new();
    }
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            let cmd = file.strip_suffix(".log").unwrap_or(&file).to_owned();
            (cmd, read_log(entry.path()).into())
        })
        .collect()
}

/// Whichever resource numbers the cgroup actually reported, or `null` when no
/// stats were collected for the test at all.
#[cfg(feature = "cgroup")]
fn resource_stats_json(stats: Option<&crate::cgroup::ResourceStats>) -> Value {
    let Some(r) = stats else {
        return Value::Null;
    };
    [
        ("cpu_user_usec", r.cpu_user_usec),
        ("cpu_system_usec", r.cpu_system_usec),
        ("memory_peak", r.memory_peak),
        ("io_read_bytes", r.io_read_bytes),
        ("io_write_bytes", r.io_write_bytes),
        ("pids_peak", r.pids_peak),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|v| (key.to_owned(), v.into())))
    .collect::<Map<String, Value>>()
    .into()
}

/// One finished test as the JSON object `--json` reports it by.
fn test_result_json(result: &TestResult) -> Value {
    let status = if result.passed {
        "pass"
    } else if result.timed_out {
        "timeout"
    } else {
        "fail"
    };
    #[cfg(feature = "cgroup")]
    let resources = resource_stats_json(result.resources.as_ref());
    #[cfg(not(feature = "cgroup"))]
    let resources = Value::Null;

    serde_json::json!({
        "name": result.name,
        "file": result.source_path.display().to_string(),
        "status": status,
        "duration_ms": result.duration.as_millis() as u64,
        "stdout": read_log(result.context.join("stdout.log")),
        "xtrace": read_log(result.context.join("xtrace.log")),
        "strace": strace_logs(&result.context),
        "resources": resources,
    })
}

pub fn print_test_result_json(result: &TestResult) {
    outln!("{}", test_result_json(result));
}

/// The one-line `PASS`/`FAIL`/`TIME` report for a finished test.
fn result_line(result: &TestResult) -> String {
    let (label, color) = if result.passed {
        ("PASS", GREEN)
    } else if result.timed_out {
        ("TIME", RED)
    } else {
        ("FAIL", RED)
    };
    let duration = format_duration(result.duration);
    let name = escape_label(&result.name);
    format!("{color}{label}{RESET}  {:<40} ({duration})", &*name)
}

pub fn print_test_result(result: &TestResult) {
    outln!("{}", result_line(result));
    #[cfg(feature = "cgroup")]
    if let Some(ref r) = result.resources {
        print_resource_stats(r);
    }
}

#[cfg(feature = "cgroup")]
fn print_resource_stats(r: &crate::cgroup::ResourceStats) {
    let mut parts: Vec<String> = Vec::new();

    match (r.cpu_user_usec, r.cpu_system_usec) {
        (Some(u), Some(s)) => parts.push(format!(
            "cpu={:.1}ms+{:.1}ms",
            u as f64 / 1000.0,
            s as f64 / 1000.0
        )),
        (Some(u), None) => parts.push(format!("cpu={:.1}ms", u as f64 / 1000.0)),
        (None, Some(s)) => parts.push(format!("cpu=sys:{:.1}ms", s as f64 / 1000.0)),
        (None, None) => {}
    }

    if let Some(m) = r.memory_peak {
        parts.push(format!("mem={}", format_bytes(m)));
    }

    match (r.io_read_bytes, r.io_write_bytes) {
        (Some(rb), Some(wb)) => parts.push(format!("io={}/{}", format_bytes(rb), format_bytes(wb))),
        (Some(rb), None) => parts.push(format!("io={}r", format_bytes(rb))),
        (None, Some(wb)) => parts.push(format!("io={}w", format_bytes(wb))),
        (None, None) => {}
    }

    if let Some(p) = r.pids_peak {
        parts.push(format!("pids={p}"));
    }

    if !parts.is_empty() {
        outln!("      {}", parts.join("  "));
    }
}

#[cfg(feature = "cgroup")]
fn format_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if b >= GIB {
        format!("{:.2}GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.1}MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.1}KiB", b as f64 / KIB as f64)
    } else {
        format!("{b}B")
    }
}

pub fn print_summary(results: &[TestResult], wall_duration: Duration) {
    let passed = results.iter().filter(|r| r.passed).count();
    let failed = results.len() - passed;

    outln!();
    if failed > 0 {
        outln!(
            "Results: {GREEN}{passed} passed{RESET}, {RED}{failed} failed{RESET}, {} total",
            results.len()
        );
    } else {
        outln!(
            "Results: {GREEN}{passed} passed{RESET}, {} total",
            results.len()
        );
    }
    outln!("Time:   {}", format_duration(wall_duration));
}

/// Format a `(file, test name)` pair as the `<file>/<test>` selector that
/// `--filter` and the positional target accept.
///
/// The file half is the path the test was discovered at, not just its base
/// name. Two things need it that way: the file half of a selector is matched
/// as a path *suffix*, so a bare `x.test/test_foo` names every `x.test` in the
/// tree while `a/x.test/test_foo` names exactly one; and a positional target
/// has to have a real path on its left (`split_path_arg` only splits where the
/// left side is an existing file), so only a selector carrying the directories
/// it was found under can be pasted back as `attest <selector>` from the
/// directory the listing was made in.
fn test_selector(file: &Path, name: &str) -> String {
    let path = file.to_string_lossy();
    // A walk rooted at `.` yields `./a/x.test`; the leading `./` is noise in a
    // selector that is already relative to the invocation directory.
    let path = path.strip_prefix("./").unwrap_or(&path);
    format!("{path}/{name}")
}

/// Print each `(file, test name)` pair as the `<file>/<test>` form `--filter`
/// and the positional target accept.
pub fn print_test_list(tests: &[(&Path, &str)]) {
    for (file, name) in tests {
        // The selector carries both halves of a name the tree chose, so it is
        // escaped as a whole rather than printed raw.
        outln!("{}", escape_label(&test_selector(file, name)));
    }
}

/// Escape the control characters in a label — a test name, or the name of a
/// file the walk turned up — before it is printed as part of the report.
///
/// Both are arbitrary bytes chosen by whoever wrote the tree, not by `attest`:
/// a shell takes almost anything as a function name (`test_x$'\e[K'() { … }`
/// defines and runs fine under `sh` and `bash`), and a file name is whatever
/// `read_dir` hands back. Printed raw, those bytes are terminal commands, and
/// the field they sit in is the part of the line that says what happened — so a
/// failing test can name itself `test_lie\e[1G\e[K\e[32mPASS\e[0m\e[2Ctest_lie`
/// and have `attest` address the cursor back over the `FAIL` it just wrote,
/// erase it, and print a green `PASS` in its place.
///
/// A label is one field *inside* a line, so `\n`, `\r` and `\t` are escaped
/// here too: a newline in a name forges a whole extra line of the report rather
/// than merely mangling the layout of one. Characters outside ASCII are left
/// alone — a UTF-8 terminal reads them as text, and a test named in Japanese
/// should still say so.
///
/// `--json` reports names verbatim: nothing interprets them there, and
/// `serde_json` escapes what its own grammar needs.
pub(crate) fn escape_label(label: &str) -> std::borrow::Cow<'_, str> {
    use std::fmt::Write as _;

    /// C0 controls and `DEL`: everything a terminal acts on rather than shows.
    fn is_control(c: char) -> bool {
        matches!(c, '\0'..='\u{1f}' | '\u{7f}')
    }

    if !label.contains(is_control) {
        return std::borrow::Cow::Borrowed(label);
    }
    let mut out = String::with_capacity(label.len());
    for c in label.chars() {
        if is_control(c) {
            let _ = write!(out, "\\x{:02x}", c as u32);
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{:.0}ms", d.as_millis())
    } else {
        format!("{secs:.2}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A finished test whose logs live in `context`.
    fn result_in(context: &Path, name: &str) -> TestResult {
        TestResult {
            name: name.to_string(),
            passed: false,
            timed_out: true,
            duration: Duration::from_millis(1500),
            context: context.to_path_buf(),
            source_path: PathBuf::from("/repo/a b.test"),
            #[cfg(feature = "cgroup")]
            resources: None,
        }
    }

    #[test]
    fn result_line_defuses_a_name_that_rewrites_the_report() {
        // The payload a hostile function name carries: address the cursor back
        // to column 1, erase the line `attest` is in the middle of writing, and
        // put a green `PASS` where the `FAIL` was. Every byte that makes that
        // work has to come out inert, and none of it may be lost — the reader
        // still has to be able to tell which test this was.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut result = result_in(
            tmp.path(),
            "test_lie\x1b[1G\x1b[K\x1b[32mPASS\x1b[0m\x1b[2Ctest_lie",
        );
        result.timed_out = false;

        assert_eq!(
            result_line(&result),
            format!(
                "{RED}FAIL{RESET}  \
                 test_lie\\x1b[1G\\x1b[K\\x1b[32mPASS\\x1b[0m\\x1b[2Ctest_lie (1.50s)"
            )
        );
    }

    #[test]
    fn escape_label_keeps_a_legible_name_as_it_is() {
        // The names attest makes up for itself (`<file>:<test>`, `#<repeat>`)
        // and any ordinary one pass through untouched, without a copy.
        assert!(matches!(
            escape_label("a.test:test_foo#2"),
            std::borrow::Cow::Borrowed(_)
        ));
        // Non-ASCII is text to a UTF-8 terminal, not a control sequence.
        assert_eq!(escape_label("testユニコード"), "testユニコード");
        // A newline would forge a whole line of the report and a tab would
        // shift the columns of one, so a label escapes those as well — unlike
        // a log, where they are the layout.
        assert_eq!(
            escape_label("test_a\nb\tc\rd\x7f"),
            r"test_a\x0ab\x09c\x0dd\x7f"
        );
    }

    #[test]
    fn json_result_survives_hostile_names_and_logs() {
        // Test names come from shell function definitions and the logs hold
        // whatever the test wrote, so both turn up carrying quotes,
        // backslashes, newlines, control bytes and invalid UTF-8. All of it
        // has to come back out of the emitted line intact: one unescaped byte
        // and the consumer's parser chokes on the run.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("stdout.log"),
            b"quote\" back\\slash \x07\xff\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("xtrace.log"), "+1: false\n").unwrap();
        std::fs::create_dir(tmp.path().join("strace")).unwrap();
        std::fs::write(
            tmp.path().join("strace/curl.log"),
            "execve(\"/bin/curl\")\n",
        )
        .unwrap();

        let result = result_in(tmp.path(), "test_\"odd\"\n\tname");
        let line = test_result_json(&result).to_string();
        let parsed: Value = serde_json::from_str(&line).expect("emitted a parseable line");

        assert_eq!(parsed["name"], json!("test_\"odd\"\n\tname"));
        assert_eq!(parsed["file"], json!("/repo/a b.test"));
        assert_eq!(parsed["status"], json!("timeout"));
        assert_eq!(parsed["duration_ms"], json!(1500));
        // The one invalid byte is replaced; everything else survives verbatim.
        assert_eq!(
            parsed["stdout"],
            json!("quote\" back\\slash \x07\u{fffd}\n")
        );
        assert_eq!(parsed["xtrace"], json!("+1: false\n"));
        assert_eq!(
            parsed["strace"],
            json!({ "curl": "execve(\"/bin/curl\")\n" })
        );
        assert_eq!(parsed["resources"], Value::Null);
    }

    #[test]
    fn json_result_keeps_every_key_when_there_is_nothing_to_report() {
        // A consumer reads the same keys for every test, so a test with no
        // logs and no --strace still gets them — empty, not missing.
        let tmp = tempfile::TempDir::new().unwrap();
        let parsed = test_result_json(&result_in(tmp.path(), "test_plain"));
        assert_eq!(parsed["stdout"], json!(""));
        assert_eq!(parsed["xtrace"], json!(""));
        assert_eq!(parsed["strace"], json!({}));
    }

    #[cfg(feature = "cgroup")]
    #[test]
    fn json_resources_omit_what_the_cgroup_did_not_report() {
        // An unavailable controller must not be reported as a real zero.
        let stats = crate::cgroup::ResourceStats {
            cpu_user_usec: Some(120),
            memory_peak: Some(4096),
            ..Default::default()
        };
        assert_eq!(
            resource_stats_json(Some(&stats)),
            json!({ "cpu_user_usec": 120, "memory_peak": 4096 })
        );
        assert_eq!(resource_stats_json(None), Value::Null);
    }

    fn display_with(total: usize, results: Vec<bool>) -> StatusDisplay {
        StatusDisplay {
            bar: None,
            total,
            results,
        }
    }

    #[test]
    fn render_blocks_zero_total_is_blank() {
        assert_eq!(display_with(0, vec![]).render_blocks(), "");
    }

    #[test]
    fn render_blocks_one_block_per_test_when_small() {
        // Fewer tests than BAR_WIDTH: one block each, colored by outcome.
        let s = display_with(2, vec![true, false]).render_blocks();
        assert_eq!(s, format!("{GREEN}█{RESET}{RED}█{RESET}"));
    }

    #[test]
    fn render_blocks_scales_to_bar_width() {
        // Many more tests than blocks: the strip stays at BAR_WIDTH blocks and
        // spans the whole run rather than growing per-test.
        let total = BAR_WIDTH * 5;
        let s = display_with(total, vec![true; total]).render_blocks();
        assert_eq!(s.matches('█').count(), BAR_WIDTH);
    }

    #[test]
    fn render_blocks_block_is_red_if_any_of_its_tests_failed() {
        // 5 tests per block; fail the first test -> only the first block is red.
        let total = BAR_WIDTH * 5;
        let mut results = vec![true; total];
        results[0] = false;
        let s = display_with(total, results).render_blocks();
        assert!(s.starts_with(&format!("{RED}█")));
        assert_eq!(s.matches(RED).count(), 1);
    }

    #[test]
    fn render_blocks_pending_blocks_are_unfilled() {
        // Only the first block's tests have finished; the rest are unfilled.
        let total = BAR_WIDTH * 2;
        let s = display_with(total, vec![true; 2]).render_blocks();
        assert!(s.contains('░'));
        assert!(s.matches('█').count() >= 1);
    }

    #[test]
    fn listed_selector_keeps_the_directories_a_test_was_found_under() {
        // A listing is relative to the directory it was made in, and a
        // positional target only splits where the left half is an existing
        // file — so the selector has to carry the path, not just the base
        // name, or `attest $(attest list . | head -1)` cannot find the file.
        assert_eq!(
            test_selector(&Path::new(".").join("tests/x.test"), "test_foo"),
            "tests/x.test/test_foo"
        );
        assert_eq!(
            test_selector(Path::new("/abs/x.test"), "test_foo"),
            "/abs/x.test/test_foo"
        );
    }

    #[test]
    fn listed_selectors_tell_same_named_files_apart() {
        // Base names repeat across a tree, and the file half of a selector is
        // a path suffix: listing base names alone printed the same line twice
        // for `a/x.test` and `b/x.test`, and that line selected both tests.
        let a = Path::new("a/x.test");
        let b = Path::new("b/x.test");
        let selector = test_selector(a, "test_foo");
        assert_ne!(selector, test_selector(b, "test_foo"));

        let pattern = crate::parser::TestPattern::parse(&selector);
        assert!(pattern.matches(a, "test_foo"));
        assert!(!pattern.matches(b, "test_foo"));
    }

    #[test]
    fn sanitize_log_defuses_cursor_control() {
        // The report-rewriting payload: move up over the `FAIL` line, erase it
        // and print a `PASS` of the test's own choosing. Every byte that makes
        // that work has to come out inert.
        let payload = b"boom\x1b[2A\x1b[2K\x1b[1;32mPASS\x1b[0m\rgone";
        assert_eq!(
            sanitize_log(payload).as_ref(),
            br"boom\x1b[2A\x1b[2K\x1b[1;32mPASS\x1b[0m\x0dgone".as_slice()
        );
        // An OSC sequence (terminal title, clipboard on some terminals) is
        // introduced and terminated by control bytes too.
        assert_eq!(
            sanitize_log(b"\x1b]0;owned\x07").as_ref(),
            br"\x1b]0;owned\x07".as_slice()
        );
    }

    #[test]
    fn sanitize_log_keeps_the_trace_readable() {
        // Newlines and tabs are what a trace is laid out with, and non-ASCII
        // bytes are text: escaping either would cost more than it buys.
        let trace = "+3: printf 'a\\tb'\ncafé\t\u{1f600}\n".as_bytes();
        assert!(matches!(sanitize_log(trace), std::borrow::Cow::Borrowed(_)));
        assert_eq!(sanitize_log(trace).as_ref(), trace);
        // Invalid UTF-8 (a test printing binary) passes through rather than
        // becoming a wall of replacement characters.
        assert_eq!(sanitize_log(b"\xff\xfe\n").as_ref(), b"\xff\xfe\n");
    }

    /// A stream whose every write fails with a fixed error kind.
    struct Failing(std::io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(self.0))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_vanished_reader_latches_instead_of_failing_the_write() {
        // The case `println!` answers with a panic: `attest | head -1`.
        let closed = AtomicBool::new(false);
        write_stream(
            &closed,
            &mut Failing(std::io::ErrorKind::BrokenPipe),
            b"PASS\n",
        );
        assert!(closed.load(Ordering::Relaxed));
    }

    #[test]
    fn other_write_errors_leave_the_run_alone() {
        // A full disk is not a reader walking away, so the run carries on
        // rather than silently reporting on a fraction of the suite.
        let closed = AtomicBool::new(false);
        write_stream(
            &closed,
            &mut Failing(std::io::ErrorKind::StorageFull),
            b"PASS\n",
        );
        assert!(!closed.load(Ordering::Relaxed));
    }

    #[test]
    fn nothing_is_written_once_the_reader_is_gone() {
        let closed = AtomicBool::new(true);
        let mut sink: Vec<u8> = Vec::new();
        write_stream(&closed, &mut sink, b"PASS\n");
        assert!(sink.is_empty());
    }
}

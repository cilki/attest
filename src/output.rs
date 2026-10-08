use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::parser::TestCase;
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
                .map(|(name, elapsed)| format!("{}({})", name, format_duration(*elapsed)))
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

/// Escape a string for inclusion as a JSON string value (without surrounding quotes).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

pub fn print_test_result_json(result: &TestResult) {
    let status = if result.passed {
        "pass"
    } else if result.timed_out {
        "timeout"
    } else {
        "fail"
    };

    // Lossy so a log containing invalid UTF-8 (e.g. binary output) is still
    // reported rather than silently becoming empty.
    let read_log = |name: &str| -> String {
        String::from_utf8_lossy(&std::fs::read(result.context.join(name)).unwrap_or_default())
            .into_owned()
    };

    let stdout = json_escape(&read_log("stdout.log"));
    let xtrace = json_escape(&read_log("xtrace.log"));

    // Collect strace logs: strace/<cmd>.log → key is <cmd>
    let strace_dir = result.context.join("strace");
    let mut strace_pairs: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&strace_dir) {
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            let key = fname.strip_suffix(".log").unwrap_or(&fname);
            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            strace_pairs.push(format!(
                "\"{}\":\"{}\"",
                json_escape(key),
                json_escape(&content)
            ));
        }
    }
    let strace_obj = format!("{{{}}}", strace_pairs.join(","));

    #[cfg(feature = "cgroup")]
    let resources_json = match result.resources {
        Some(ref r) => {
            let mut fields: Vec<String> = Vec::new();
            if let Some(v) = r.cpu_user_usec {
                fields.push(format!("\"cpu_user_usec\":{v}"));
            }
            if let Some(v) = r.cpu_system_usec {
                fields.push(format!("\"cpu_system_usec\":{v}"));
            }
            if let Some(v) = r.memory_peak {
                fields.push(format!("\"memory_peak\":{v}"));
            }
            if let Some(v) = r.io_read_bytes {
                fields.push(format!("\"io_read_bytes\":{v}"));
            }
            if let Some(v) = r.io_write_bytes {
                fields.push(format!("\"io_write_bytes\":{v}"));
            }
            if let Some(v) = r.pids_peak {
                fields.push(format!("\"pids_peak\":{v}"));
            }
            format!("{{{}}}", fields.join(","))
        }
        None => "null".to_string(),
    };
    #[cfg(not(feature = "cgroup"))]
    let resources_json = "null";

    let name = json_escape(&result.name);
    let file = json_escape(&result.source_path.display().to_string());
    let duration_ms = result.duration.as_millis();

    outln!(
        r#"{{"name":"{name}","file":"{file}","status":"{status}","duration_ms":{duration_ms},"stdout":"{stdout}","xtrace":"{xtrace}","strace":{strace_obj},"resources":{resources_json}}}"#
    );
}

pub fn print_test_result(result: &TestResult) {
    let (label, color) = if result.passed {
        ("PASS", GREEN)
    } else if result.timed_out {
        ("TIME", RED)
    } else {
        ("FAIL", RED)
    };
    let duration = format_duration(result.duration);
    outln!("{color}{label}{RESET}  {:<40} ({duration})", result.name);
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

pub fn print_test_list(tests: &[TestCase]) {
    for test in tests {
        let filename = test
            .file
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| test.file.to_string_lossy());
        outln!("{}/{}", filename, test.name);
    }
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

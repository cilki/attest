use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;

use crate::runner::{Outcome, TestResult};

pub(crate) const GREEN: &str = "\x1b[32m";
pub(crate) const RED: &str = "\x1b[31m";
pub(crate) const DIM: &str = "\x1b[2m";
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

/// Stdout as a [`Write`], so something that writes incrementally (the
/// `--json` serializer) can go straight out instead of being assembled in
/// memory first. Every write goes through [`write_out`], so a vanished reader
/// still latches [`BROKEN_PIPE`] rather than failing the write.
struct StdoutSink;

impl Write for StdoutSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        write_out(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // `write_out` flushes what it writes.
        Ok(())
    }
}

pub fn write_err(buf: &[u8]) {
    write_stream(&BROKEN_PIPE, &mut std::io::stderr().lock(), buf);
}

/// Replay a captured log to stderr, dimmed, with the bytes that drive a
/// terminal escaped, so replaying it cannot rewrite the report around it.
/// Returns how many bytes of `log` were consumed.
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
/// `last` says there will be no more of the log. Until then a trailing
/// [`C1_LEAD`] is left unconsumed, because an encoded C1 control is two bytes
/// and a test is free to write them one at a time — `printf '\302'; sleep 1;
/// printf '\233'` puts them either side of a chunk boundary, where sanitizing
/// each chunk on its own would pass both halves through and hand the terminal
/// a `CSI`.
///
/// The full bytes are still reported verbatim where nothing interprets them:
/// `--json` and `--save-context`.
pub fn replay_log_err(log: &mut impl Read, last: bool) -> u64 {
    stream_log(&BROKEN_PIPE, &mut std::io::stderr().lock(), log, last)
}

/// How much of a log is held in memory at a time while it is replayed.
const LOG_CHUNK: usize = 64 * 1024;

/// Replay `log` to `stream` a [`LOG_CHUNK`] at a time, wrapping whatever it
/// prints in [`DIM`]. Returns the number of bytes consumed: all of them, bar a
/// trailing [`C1_LEAD`] when `last` is false.
///
/// Bounded rather than read whole because a log is as big as the test that
/// wrote it chose to make it: `xtrace.log` carries everything the test and the
/// programs it ran sent to stderr, and a test that fails after a verbose build
/// spewed a few hundred megabytes can easily have left a bigger log than the
/// machine has memory to hold — twice over, since escaping allocates a copy of
/// up to four bytes per byte. Reading it in one go made one noisy test enough
/// to get `attest` OOM-killed, losing the results of every *other* test in the
/// run along with it.
fn stream_log(
    closed: &AtomicBool,
    stream: &mut impl Write,
    log: &mut impl Read,
    last: bool,
) -> u64 {
    /// Open the dimmed region, once, and only if there is anything to put in
    /// it: a flush that finds nothing new must print nothing at all.
    fn open_dim(closed: &AtomicBool, stream: &mut impl Write, opened: &mut bool) {
        if !std::mem::replace(opened, true) {
            write_stream(closed, stream, DIM.as_bytes());
        }
    }

    let mut buf = vec![0u8; LOG_CHUNK];
    let mut consumed = 0u64;
    // A `C1_LEAD` the previous chunk ended on, left for this one: it goes at
    // the front of the buffer, so the pair it may be the first half of is
    // sanitized as a pair rather than a chunk boundary apart.
    let mut carry = false;
    let mut opened = false;
    loop {
        // Nowhere left to replay it to (`attest | head -1`): stop reading
        // rather than walking the whole log to write none of it.
        if closed.load(Ordering::Relaxed) {
            break;
        }
        let start = usize::from(carry);
        if carry {
            buf[0] = C1_LEAD;
        }
        let n = match log.read(&mut buf[start..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        consumed += n as u64;
        let mut end = start + n;
        carry = buf[end - 1] == C1_LEAD;
        if carry {
            end -= 1;
        }
        // The whole chunk was the lead byte now being held back.
        if end == 0 {
            continue;
        }
        open_dim(closed, stream, &mut opened);
        write_stream(closed, stream, &sanitize_log(&buf[..end]));
    }
    if carry {
        if last {
            // Nothing follows it after all, so it was never half of anything.
            open_dim(closed, stream, &mut opened);
            write_stream(closed, stream, &sanitize_log(&[C1_LEAD]));
        } else {
            consumed -= 1; // left for the call that has the byte after it
        }
    }
    if opened {
        write_stream(closed, stream, RESET.as_bytes());
    }
    consumed
}

/// Whether `byte` on its own has to be escaped before being replayed into a
/// terminal: every C0 control byte bar the `\n` and `\t` a trace legitimately
/// contains, plus `DEL`.
///
/// A raw byte in `0x80..=0xbf` is left alone: it is not valid UTF-8 on its own,
/// so a UTF-8 terminal does not read it as a C1 control, and most of them are
/// the continuation bytes of ordinary non-ASCII text (`日` is `e6 97 a5`, whose
/// `97` sits squarely in the C1 range). The *encoded* C1 controls are a
/// different matter — see [`C1_LEAD`].
fn needs_escape(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0b..=0x1f | 0x7f)
}

/// Lead byte of the two-byte UTF-8 encoding of a C1 control, `U+0080..=U+009F`
/// (`c2 80` … `c2 9f`).
///
/// A terminal in UTF-8 mode decodes those code points and then *acts* on them:
/// they are the single-byte forms of the escape sequences, and `U+009B` (`CSI`)
/// stands in for `ESC [` exactly. xterm spells this out — its `allowC1Printable`
/// resource, off by default, is what "overrides the mapping of C1 control
/// characters (code 128-159) to treat them as printable" — and the VTE-based
/// terminals do the same. So escaping only `ESC` leaves the whole of
/// [`replay_log_err`]'s job undone: `\u{9b}2K` erases a line just as `\x1b[2K`
/// does.
///
/// `0xc2` can never be a continuation byte (those are `0x80..=0xbf`), so the
/// pair is unambiguous: `c2` followed by `80..=9f` is always one encoded C1
/// control and never part of some longer character.
pub(crate) const C1_LEAD: u8 = 0xc2;

/// Does `buf` begin with the UTF-8 encoding of a C1 control? See [`C1_LEAD`].
fn starts_with_c1(buf: &[u8]) -> bool {
    matches!(buf, [C1_LEAD, 0x80..=0x9f, ..])
}

/// Replace each byte [`needs_escape`] rejects, and each encoded C1 control,
/// with a `\xNN` spelling of it, borrowing the input unchanged when there is
/// nothing to escape (the usual case, since a trace is mostly printable text).
///
/// Escaped byte-wise rather than over `char`s: a log is arbitrary bytes and
/// need not be valid UTF-8, and lossy decoding would turn a test's binary
/// output into a wall of U+FFFD. Backslashes are left as they are — bash
/// xtrace is full of them (`$'a\nb'`) and doubling every one would be a worse
/// trade than the ambiguity with a literal `\x1b` in the log text.
fn sanitize_log(buf: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    // A `0xc2` that turns out not to lead a C1 control only costs the slow
    // path, which copies it through unchanged.
    if !buf.iter().copied().any(|b| needs_escape(b) || b == C1_LEAD) {
        return std::borrow::Cow::Borrowed(buf);
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut rest = buf;
    while let Some((&byte, tail)) = rest.split_first() {
        if starts_with_c1(rest) {
            out.extend_from_slice(format!("\\x{C1_LEAD:02x}\\x{:02x}", rest[1]).as_bytes());
            rest = &rest[2..];
        } else if needs_escape(byte) {
            out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
            rest = tail;
        } else {
            out.push(byte);
            rest = tail;
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
    ///
    /// How many tests have finished is [`StatusDisplay::record`]'s to say, not
    /// the caller's: the bar's position was set from the caller's own count of
    /// results as well, which is the same number by construction and one more
    /// thing to keep that way.
    pub fn update(&self, running: &[(&str, Duration)]) {
        if let Some(ref bar) = self.bar {
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

/// One of a test's logs as a JSON string, read out of its context dir while it
/// is being written out rather than beforehand.
///
/// `serde_json`'s `collect_str` escapes and emits a string a fragment at a
/// time, so a log reaches the output stream a [`LOG_CHUNK`] at a time and never
/// has to fit in memory — the same reason [`stream_log`] replays one in chunks.
/// Holding it whole is how one test that fails after printing a few hundred
/// megabytes used to get `attest` OOM-killed under `--json`, losing the results
/// of every *other* test in the run with it.
///
/// Opened with [`crate::overlay::open_nofollow`]: a test's context dir is live,
/// so it can put a symlink where its log was and a plain open would hand a JSON
/// consumer the contents of an arbitrary host file as the test's output. A log
/// that cannot be opened reports as the empty string.
struct LogField(PathBuf);

impl serde::Serialize for LogField {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&LogText(&self.0))
    }
}

/// The text of a log, written out a chunk at a time. Lossy so a log holding
/// invalid UTF-8 (a test printing binary) is still reported rather than
/// silently becoming empty.
struct LogText<'a>(&'a Path);

impl std::fmt::Display for LogText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Ok(mut log) = crate::overlay::open_nofollow(self.0) else {
            return Ok(());
        };
        // Room for one read plus the up-to-three bytes of a character the
        // previous read stopped in the middle of.
        let mut buf = vec![0u8; LOG_CHUNK + 3];
        let mut held = 0usize;
        loop {
            let n = match log.read(&mut buf[held..held + LOG_CHUNK]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            held = write_lossy(f, &mut buf, held + n)?;
        }
        // Nothing follows the bytes still held back, so they were truncated by
        // the end of the log rather than by a chunk boundary after all.
        if held > 0 {
            f.write_str("\u{fffd}")?;
        }
        Ok(())
    }
}

/// Write `buf[..len]` as text, standing in `U+FFFD` for each invalid sequence
/// the way [`String::from_utf8_lossy`] does.
///
/// Returns how many trailing bytes were *not* written because they may be the
/// start of a character the next chunk completes, having moved them to the
/// front of `buf`. Decoding them on their own would spell each one `U+FFFD` and
/// lose the character: the chunk boundary falls wherever the log's length puts
/// it, which is not a place the test has to avoid.
fn write_lossy(
    f: &mut std::fmt::Formatter<'_>,
    buf: &mut [u8],
    len: usize,
) -> Result<usize, std::fmt::Error> {
    let mut start = 0;
    while start < len {
        let e = match std::str::from_utf8(&buf[start..len]) {
            Ok(text) => {
                f.write_str(text)?;
                start = len;
                break;
            }
            Err(e) => e,
        };
        let good = e.valid_up_to();
        if good > 0 {
            // Valid by construction: `valid_up_to` is where it stops being so.
            f.write_str(std::str::from_utf8(&buf[start..start + good]).unwrap_or_default())?;
            start += good;
        }
        match e.error_len() {
            // Invalid however the log continues, so it is reported now.
            Some(bad) => {
                f.write_str("\u{fffd}")?;
                start += bad;
            }
            // Cut short by the end of the chunk: hold it for the next one.
            None => break,
        }
    }
    buf.copy_within(start..len, 0);
    Ok(len - start)
}

/// A test's `--strace` logs as `{"<cmd>": "<log>"}`, read from the
/// `strace/<cmd>.log` files the runner wrote into its context dir. Empty
/// without `--strace` (or when nothing was traced).
fn strace_logs(context: &Path) -> BTreeMap<String, LogField> {
    let dir = context.join("strace");
    if !crate::overlay::is_real_dir(&dir) {
        return BTreeMap::new();
    }
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            let cmd = file.strip_suffix(".log").unwrap_or(&file).to_owned();
            (cmd, LogField(entry.path()))
        })
        .collect()
}

/// Whichever resource numbers the cgroup actually reported, or `null` when no
/// stats were collected for the test at all.
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
    .collect::<serde_json::Map<String, Value>>()
    .into()
}

/// One finished test as the JSON object `--json` reports it by.
///
/// Serialized straight to the output stream rather than built up as a
/// [`Value`]: three of its fields are whole logs, and a log is as big as the
/// test that wrote it chose to make it (see [`LogField`]).
struct TestResultJson<'a>(&'a TestResult);

impl serde::Serialize for TestResultJson<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let result = self.0;
        let status = match result.outcome {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Timeout => "timeout",
        };
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", &result.name)?;
        map.serialize_entry("file", &result.source_path.display().to_string())?;
        map.serialize_entry("status", status)?;
        map.serialize_entry("duration_ms", &(result.duration.as_millis() as u64))?;
        map.serialize_entry("stdout", &LogField(result.context.join("stdout.log")))?;
        map.serialize_entry("xtrace", &LogField(result.context.join("xtrace.log")))?;
        map.serialize_entry("strace", &strace_logs(&result.context))?;
        map.serialize_entry("resources", &resource_stats_json(result.resources.as_ref()))?;
        map.end()
    }
}

/// Write the one JSONL line for `result` to `out`.
fn write_test_result_json(result: &TestResult, out: &mut impl Write) -> std::io::Result<()> {
    let mut serializer = serde_json::Serializer::new(&mut *out);
    serde::Serialize::serialize(&TestResultJson(result), &mut serializer)
        .map_err(std::io::Error::other)?;
    out.write_all(b"\n")
}

pub fn print_test_result_json(result: &TestResult) {
    // Buffered: `serde_json` emits a string in fragments split at every byte it
    // has to escape, and an xtrace log has a `\n` on most of its lines, so
    // unbuffered this would be two writes (and two flushes) per line of log.
    let mut out = std::io::BufWriter::with_capacity(LOG_CHUNK, StdoutSink);
    let _ = write_test_result_json(result, &mut out);
    let _ = out.flush();
}

/// The one-line `PASS`/`FAIL`/`TIME` report for a finished test.
fn result_line(result: &TestResult) -> String {
    let (label, color) = match result.outcome {
        Outcome::Pass => ("PASS", GREEN),
        Outcome::Fail => ("FAIL", RED),
        Outcome::Timeout => ("TIME", RED),
    };
    let duration = format_duration(result.duration);
    let name = escape_label(&result.name);
    format!("{color}{label}{RESET}  {:<40} ({duration})", &*name)
}

pub fn print_test_result(result: &TestResult) {
    outln!("{}", result_line(result));
    if let Some(ref r) = result.resources {
        print_resource_stats(r);
    }
}

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
    let passed = results.iter().filter(|r| r.outcome.passed()).count();
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

/// Print each `(file, test name)` pair as the `<file>/<test>` form `--filter`
/// and the positional target accept (see [`crate::parser::selector`], which
/// owns what that form is).
pub fn print_test_list(tests: &[(&Path, &str)]) {
    for (file, name) in tests {
        // The selector carries both halves of a name the tree chose, so it is
        // escaped as a whole rather than printed raw.
        outln!("{}", escape_label(&crate::parser::selector(file, name)));
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
/// than merely mangling the layout of one. Printable characters outside ASCII
/// are left alone — a UTF-8 terminal reads them as text, and a test named in
/// Japanese should still say so.
///
/// `--json` reports names verbatim: nothing interprets them there, and
/// `serde_json` escapes what its own grammar needs.
pub(crate) fn escape_label(label: &str) -> std::borrow::Cow<'_, str> {
    /// C0 controls, `DEL` and the C1 controls: everything a terminal acts on
    /// rather than shows. The C1 block matters because a terminal in UTF-8
    /// mode decodes those code points and obeys them — `U+009B` is `CSI`, so
    /// `test_lie\u{9b}1G\u{9b}K` reaches back over the verdict next to it
    /// exactly as the `ESC [` spelling does (see [`C1_LEAD`]).
    fn is_control(c: char) -> bool {
        matches!(c, '\0'..='\u{1f}' | '\u{7f}'..='\u{9f}')
    }

    escape_chars(label, is_control)
}

/// Spell every character `escape` picks out as `\xNN`, borrowing `s` unchanged
/// when it holds none of them — which is the usual case, since the text this
/// runs over is a name or a line of shell.
///
/// Who has to escape what differs, so the predicate is the caller's: a label
/// attest prints itself has to defuse every control character
/// ([`escape_label`]), while a source line reaches the terminal through
/// `annotate_snippets`, which already substitutes the C0 controls and leaves
/// only the C1 block to deal with (`diagnostics::escape_c1`).
pub(crate) fn escape_chars(s: &str, escape: fn(char) -> bool) -> std::borrow::Cow<'_, str> {
    use std::fmt::Write as _;

    if !s.contains(escape) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if escape(c) {
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

    /// The JSONL line `--json` prints for `result`.
    fn json_line(result: &TestResult) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        write_test_result_json(result, &mut out).expect("serializing a result");
        out
    }

    /// The same line, parsed back. Going through the real writer rather than a
    /// `Value` is the point: the logs are streamed into it as it is written,
    /// so nothing else exercises the chunking.
    fn test_result_json(result: &TestResult) -> Value {
        let line = json_line(result);
        assert!(
            line.ends_with(b"\n"),
            "a JSONL line has to end in a newline"
        );
        serde_json::from_slice(&line).expect("emitted a parseable line")
    }

    /// A finished test whose logs live in `context`.
    fn result_in(context: &Path, name: &str) -> TestResult {
        TestResult {
            name: name.to_string(),
            outcome: Outcome::Timeout,
            duration: Duration::from_millis(1500),
            context: context.to_path_buf(),
            source_path: PathBuf::from("/repo/a b.test"),
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
        result.outcome = Outcome::Fail;

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
    fn escape_label_defuses_a_name_spelled_with_c1_controls() {
        // The verdict-forging name of `result_line_escapes_a_hostile_name`,
        // rewritten with `U+009B` (`CSI`) in place of every `ESC [`. A UTF-8
        // terminal decodes those code points and acts on them, so leaving them
        // alone as "non-ASCII text" hands the test back the cursor.
        assert_eq!(
            escape_label("test_lie\u{9b}1G\u{9b}K\u{9b}32mPASS\u{9b}0m"),
            r"test_lie\x9b1G\x9bK\x9b32mPASS\x9b0m"
        );
        // The rest of the block goes too: `U+0085` is `NEL`, a line break of
        // its own, and `U+009D` opens an OSC string.
        assert_eq!(
            escape_label("test_a\u{85}\u{9d}0;owned"),
            r"test_a\x85\x9d0;owned"
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
        let parsed = test_result_json(&result);

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

    #[test]
    fn a_json_log_too_big_for_one_chunk_is_reported_whole() {
        // Logs go into the line a bounded chunk at a time (a test can leave one
        // bigger than memory), so the chunking must be invisible in the result:
        // every character, once, in order. The payload carries the bytes
        // `serde_json` escapes, so fragments are split inside chunks too.
        let tmp = tempfile::TempDir::new().unwrap();
        let log: String = (0..LOG_CHUNK * 2 + 7)
            .map(|i| match i % 5 {
                0 => '\n',
                1 => '"',
                2 => '\\',
                3 => '\u{7}',
                _ => 'x',
            })
            .collect();
        std::fs::write(tmp.path().join("stdout.log"), &log).unwrap();

        let parsed = test_result_json(&result_in(tmp.path(), "test_big"));
        assert_eq!(parsed["stdout"], json!(log));
    }

    #[test]
    fn a_character_straddling_a_json_chunk_survives_it() {
        // The read boundary falls wherever the log's length puts it, which is
        // not somewhere the test has to avoid. Decoding each chunk on its own
        // turns both halves of a character sitting across one into U+FFFD, so
        // a log with a single emoji in the wrong place came out corrupt.
        let tmp = tempfile::TempDir::new().unwrap();
        for lead in 1..=3 {
            // Place the character so that `lead` of its bytes land in the first
            // chunk and the rest in the second.
            let mut log = "x".repeat(LOG_CHUNK - lead);
            log.push('\u{1f600}'); // four bytes
            log.push_str("tail\n");
            std::fs::write(tmp.path().join("xtrace.log"), &log).unwrap();

            let parsed = test_result_json(&result_in(tmp.path(), "test_split"));
            assert_eq!(parsed["xtrace"], json!(log), "split after {lead} byte(s)");
        }
    }

    #[test]
    fn a_json_log_cut_off_mid_character_ends_in_one_replacement() {
        // A log is whatever the test wrote, and a test killed by `--timeout`
        // mid-`printf` leaves a truncated character at the end of it. That one
        // is never completed, so it reports as a single U+FFFD — the same as
        // reading the log whole and converting it lossily.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("stdout.log"), b"ok\xf0\x9f\x98").unwrap();

        let parsed = test_result_json(&result_in(tmp.path(), "test_cut"));
        assert_eq!(parsed["stdout"], json!("ok\u{fffd}"));
    }

    #[test]
    fn a_symlinked_json_log_reports_as_empty() {
        // A test's context dir is live, so it can delete `stdout.log` and leave
        // a link to a host file at the name. Streaming the log must not resolve
        // that any more than reading it whole did: a JSON consumer would be fed
        // the file's contents as the test's own output.
        let tmp = tempfile::TempDir::new().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();
        let secret = elsewhere.path().join("secret");
        std::fs::write(&secret, "SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, tmp.path().join("stdout.log")).unwrap();

        let parsed = test_result_json(&result_in(tmp.path(), "test_link"));
        assert_eq!(parsed["stdout"], json!(""));
    }

    #[test]
    fn json_resources_omit_what_the_cgroup_did_not_report() {
        // An unavailable controller must not be reported as a real zero, so
        // the keys it would have filled are absent from the object rather
        // than present and zero. Asserted through the emitted line, which is
        // the only thing a consumer ever sees.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut result = result_in(tmp.path(), "test_stats");
        result.resources = Some(crate::cgroup::ResourceStats {
            cpu_user_usec: Some(120),
            memory_peak: Some(4096),
            ..Default::default()
        });
        assert_eq!(
            test_result_json(&result)["resources"],
            json!({ "cpu_user_usec": 120, "memory_peak": 4096 })
        );

        // No cgroup at all is `null`, not an empty object: a consumer can tell
        // "nothing was measured" from "measured and reported nothing".
        result.resources = None;
        assert_eq!(test_result_json(&result)["resources"], Value::Null);
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
            crate::parser::selector(&Path::new(".").join("tests/x.test"), "test_foo"),
            "tests/x.test/test_foo"
        );
        assert_eq!(
            crate::parser::selector(Path::new("/abs/x.test"), "test_foo"),
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
        let selector = crate::parser::selector(a, "test_foo");
        assert_ne!(selector, crate::parser::selector(b, "test_foo"));

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

    #[test]
    fn sanitize_log_defuses_encoded_c1_controls() {
        // `U+009B` is `CSI`: the same payload as above, spelled with the
        // single-code-point C1 controls a UTF-8 terminal decodes and obeys
        // just the same. Escaping only `ESC` would let every one of these
        // through.
        assert_eq!(
            sanitize_log("boom\u{9b}2A\u{9b}2K\u{9b}1;32mPASS\u{9b}0m".as_bytes()).as_ref(),
            br"boom\xc2\x9b2A\xc2\x9b2K\xc2\x9b1;32mPASS\xc2\x9b0m".as_slice()
        );
        // `U+009D` is `OSC`, and `U+009C` the string terminator that closes it.
        assert_eq!(
            sanitize_log("\u{9d}0;owned\u{9c}".as_bytes()).as_ref(),
            br"\xc2\x9d0;owned\xc2\x9c".as_slice()
        );
    }

    #[test]
    fn sanitize_log_keeps_non_ascii_text_that_is_not_a_c1_control() {
        // Only `c2` followed by `80..=9f` is an encoded C1 control. A `c2`
        // leading anything else is ordinary text (`¡` is `c2 a1`)...
        assert_eq!(
            sanitize_log("¡olé!\n".as_bytes()).as_ref(),
            "¡olé!\n".as_bytes()
        );
        // ...and a byte in the C1 range that is *not* behind a `c2` is a
        // continuation byte of some other character, not a control: `日` is
        // `e6 97 a5`, and mangling its `97` would wreck every log with CJK text
        // in it.
        assert_eq!(
            sanitize_log("日本語\n".as_bytes()).as_ref(),
            "日本語\n".as_bytes()
        );
        // A lone `c2` (truncated or binary output) is not a control either.
        assert_eq!(sanitize_log(b"\xc2").as_ref(), b"\xc2");
        // Pairs are taken two bytes at a time, so a `c2` that only leads
        // another `c2` stays as it is and the pair behind it is still caught.
        let mut expected = vec![C1_LEAD];
        expected.extend_from_slice(br"\xc2\x9b");
        assert_eq!(sanitize_log(b"\xc2\xc2\x9b").as_ref(), expected.as_slice());
    }

    /// Replay `log` into a buffer, the way [`replay_log_err`] replays it into
    /// stderr. Returns what was written and how much of `log` was consumed.
    fn replayed(log: &[u8], last: bool) -> (Vec<u8>, u64) {
        let closed = AtomicBool::new(false);
        let mut sink: Vec<u8> = Vec::new();
        let consumed = stream_log(&closed, &mut sink, &mut std::io::Cursor::new(log), last);
        (sink, consumed)
    }

    /// The same, for a replay whose output is text (everything but a lone
    /// [`C1_LEAD`] comes out as ASCII or as the bytes of the log).
    fn replayed_text(log: &[u8], last: bool) -> (String, u64) {
        let (out, consumed) = replayed(log, last);
        (
            String::from_utf8(out).expect("escaped output is text"),
            consumed,
        )
    }

    #[test]
    fn a_log_is_replayed_wrapped_in_one_dimmed_region() {
        let (out, consumed) = replayed_text(b"+1: false\n", true);
        assert_eq!(out, format!("{DIM}+1: false\n{RESET}"));
        assert_eq!(consumed, 10);
    }

    #[test]
    fn replaying_nothing_prints_nothing() {
        // The live tail flushes on every poll, so most flushes find no new
        // bytes: those must not each leave a stray escape sequence behind.
        assert_eq!(replayed(b"", false), (Vec::new(), 0));
    }

    #[test]
    fn a_log_too_big_for_one_chunk_is_replayed_whole() {
        // Logs are read a bounded chunk at a time (a test can leave one bigger
        // than memory), so the chunking must be invisible in the result: every
        // byte, once, in order, inside a single dimmed region.
        let log: Vec<u8> = (0..LOG_CHUNK * 2 + 7)
            .map(|i| b'a' + (i % 26) as u8)
            .collect();
        let (out, consumed) = replayed_text(&log, true);
        assert_eq!(consumed, log.len() as u64);
        assert_eq!(
            out,
            format!("{DIM}{}{RESET}", String::from_utf8_lossy(&log))
        );
    }

    #[test]
    fn an_encoded_c1_control_split_across_a_chunk_is_still_escaped() {
        // The chunk boundary is attacker-chosen: a test can place the two
        // bytes of `U+009B` (`CSI`) either side of it, and a chunk sanitized
        // on its own passes a lone `c2` through as text and a lone `9b` as a
        // continuation byte — handing the terminal the control they spell
        // together.
        let mut log = vec![b'.'; LOG_CHUNK - 1];
        log.extend_from_slice(b"\xc2\x9b2K");
        let (out, consumed) = replayed_text(&log, true);
        assert_eq!(consumed, log.len() as u64);
        assert!(!out.contains('\u{9b}'), "a CSI reached the terminal");
        assert_eq!(
            out,
            format!("{DIM}{}\\xc2\\x9b2K{RESET}", ".".repeat(LOG_CHUNK - 1))
        );
    }

    #[test]
    fn a_trailing_lead_byte_waits_for_the_byte_after_it() {
        // Same payload, arriving a byte at a time down a live tail: the `c2`
        // ends the log for now, so it is neither printed nor counted as read,
        // and the next flush starts again from it.
        let (out, consumed) = replayed_text(b"ok\xc2", false);
        assert_eq!(out, format!("{DIM}ok{RESET}"));
        assert_eq!(consumed, 2, "the lead byte must be re-read next time");

        // Once the test has finished there is no byte after it after all, and
        // a lone `c2` is not a control: it goes out as it is.
        let (out, consumed) = replayed(b"ok\xc2", true);
        assert_eq!(out, [DIM.as_bytes(), b"ok\xc2", RESET.as_bytes()].concat());
        assert_eq!(consumed, 3);
    }

    #[test]
    fn nothing_is_read_once_the_reader_is_gone() {
        // `attest | head -1` on a run with a huge log: there is nobody left to
        // print it to, so it must not be walked to print none of it.
        let closed = AtomicBool::new(true);
        let mut sink: Vec<u8> = Vec::new();
        let mut log = std::io::Cursor::new(vec![b'x'; LOG_CHUNK * 4]);
        assert_eq!(stream_log(&closed, &mut sink, &mut log, true), 0);
        assert!(sink.is_empty());
        assert_eq!(log.position(), 0);
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

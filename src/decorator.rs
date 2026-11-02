//! `#attest(...)` decorators: conditions written in a comment block directly
//! above a test function, checked against the run of that test.
//!
//! ```sh
//! # attest(2ms < cpu < 10ms, mem < 50MiB)
//! testHello() {
//!     ...
//! }
//! ```
//!
//! A block is one or more consecutive comment lines: it opens with `#attest(`
//! (blanks are allowed around `attest`, and there must be exactly one `#` — a
//! `##` line is a doc comment, the README's own convention for describing a
//! test) and closes at the first `)` outside braces. Every line in between is
//! a comment whose `#` is stripped before the text is read, so the same block
//! can be written on one line or spread over several:
//!
//! ```sh
//! # attest(
//! #   2ms < cpu < 10ms,
//! #   pids <= 3,
//! # )
//! ```
//!
//! Conditions are separated by commas, a trailing comma is allowed, and every
//! one of them is a predicate over one run of the test:
//!
//! ```text
//! condition := METRIC OP QTY | QTY OP METRIC | QTY OP METRIC OP QTY
//!            | 'syscalls' ('in' | 'not in') set
//! |          | 'hosts' 'in' set
//! |          | 'writes' 'in' set
//! METRIC    := cpu | wall | mem | io_read | io_write | pids
//! OP        := '<' | '<=' | '>' | '>='
//! QTY       := number ('.' number)? UNIT?
//! set       := '{' (item (',' item)* ','?)? '}'
//! ```
//!
//! The unit family is fixed by the metric: durations (`us`, `ms`, `s`) for
//! `cpu` and `wall`, bytes (`B`, `KiB`, `MiB`, `GiB`, `KB`, `MB`, `GB`) for
//! `mem`, `io_read` and `io_write`, and a bare whole number for `pids`. A
//! chain (`2ms < cpu < 10ms`) must point the same way throughout and describe
//! a non-empty range.
//!
//! Comments never reach the shell parser — brush drops them in the tokenizer
//! — so a block is found by scanning the file's text ([`scan`]) and attached
//! to the function the parser found directly below it (see
//! `parser::parse_test_file`). Any mistake in a block is a parse error of the
//! file, reported the way an unparseable script is: an assertion that
//! silently did nothing would be worse than one that is loud about being
//! malformed. The `syscalls`, `hosts` and `writes` forms are accepted by the
//! grammar but not enforced yet, and so are rejected the same way
//! ([`Decorator::check_supported`]) rather than being quietly ignored.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use syscalls::Sysno;

use crate::cgroup::ResourceStats;

/// A measurable property of one run of a test.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Metric {
    /// CPU time, user plus system, from the test's cgroup.
    Cpu,
    /// Wall-clock time from spawn to exit.
    Wall,
    /// Peak memory from the test's cgroup.
    Mem,
    /// Bytes read from block devices, from the test's cgroup.
    IoRead,
    /// Bytes written to block devices, from the test's cgroup.
    IoWrite,
    /// Largest number of processes alive at once, from the test's cgroup.
    Pids,
}

impl Metric {
    /// The metrics `--add-decorators` writes a condition for, in the order
    /// it writes them. The stable ones: `wall` is whatever the scheduler made
    /// of the run (a 4ms test takes 40ms on a loaded machine), and `io_*` is
    /// usually zero and rarely what a test is about.
    pub const GENERATED: [Metric; 3] = [Metric::Cpu, Metric::Mem, Metric::Pids];

    /// Whether a generated upper bound on this metric is written as `<=`
    /// rather than `<`: a count is bounded by the value itself (`pids <= 3`),
    /// a measurement by a value with headroom above it.
    pub fn generated_inclusive(self) -> bool {
        self.unit() == Unit::Count
    }

    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "cpu" => Metric::Cpu,
            "wall" => Metric::Wall,
            "mem" => Metric::Mem,
            "io_read" => Metric::IoRead,
            "io_write" => Metric::IoWrite,
            "pids" => Metric::Pids,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Metric::Cpu => "cpu",
            Metric::Wall => "wall",
            Metric::Mem => "mem",
            Metric::IoRead => "io_read",
            Metric::IoWrite => "io_write",
            Metric::Pids => "pids",
        }
    }

    /// Which family of units a quantity compared against this metric is in.
    pub fn unit(self) -> Unit {
        match self {
            Metric::Cpu | Metric::Wall => Unit::Duration,
            Metric::Mem | Metric::IoRead | Metric::IoWrite => Unit::Bytes,
            Metric::Pids => Unit::Count,
        }
    }

    /// Where the number comes from, for the "could not be measured" warning.
    pub fn source(self) -> &'static str {
        match self {
            Metric::Wall => "the clock",
            Metric::Cpu => "the cgroup cpu controller",
            Metric::Mem => "the cgroup memory controller",
            Metric::IoRead | Metric::IoWrite => "the cgroup io controller",
            Metric::Pids => "the cgroup pids controller",
        }
    }

    /// This metric's value for a run, in its base unit (microseconds, bytes
    /// or a count), or `None` when the run did not measure it.
    pub fn measure(self, sample: &Sample) -> Option<u64> {
        let r = sample.resources;
        match self {
            Metric::Wall => Some(sample.wall.as_micros().try_into().unwrap_or(u64::MAX)),
            // The same "user plus system" the resource line reports: a
            // condition on `cpu` is checked against the number the user sees.
            Metric::Cpu => {
                let r = r?;
                Some(r.cpu_user_usec? + r.cpu_system_usec?)
            }
            Metric::Mem => r?.memory_peak,
            Metric::IoRead => r?.io_read_bytes,
            Metric::IoWrite => r?.io_write_bytes,
            Metric::Pids => r?.pids_peak,
        }
    }
}

/// What a run measured: the inputs every condition is checked against.
///
/// Each half is `None` where the run has no number for it, and a condition
/// on that half comes out [`Verdict::Unmeasured`] rather than as a pass: the
/// cgroup may not have been creatable, the syscall listener may not be
/// supported by the kernel, and the delta is only walked for a test with a
/// `writes` condition.
pub struct Sample<'a> {
    pub wall: Duration,
    pub resources: Option<&'a ResourceStats>,
    /// Every syscall the test's process tree made that the run watched for.
    pub syscalls: Option<&'a SyscallLog>,
    /// Every absolute path the test created, modified or deleted in its
    /// ephemeral root (see `overlay::delta_paths`).
    pub writes: Option<&'a [PathBuf]>,
    /// The invocation dir, which a `writes` item spelled `./x` is relative
    /// to.
    pub project: &'a Path,
}

/// The syscalls a run was watched for and made: each number with how many
/// times it was seen. Only the syscalls the test's conditions name (or, for
/// an allow-list, everything they do not) are ever counted — nothing watches
/// a test that has no `syscalls` condition.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SyscallLog {
    pub counts: BTreeMap<Sysno, u64>,
}

impl SyscallLog {
    pub fn record(&mut self, nr: Sysno) {
        *self.counts.entry(nr).or_insert(0) += 1;
    }
}

/// Where a condition's measurement comes from, for the "could not be
/// measured" warning: the quantity metrics each from their own counter, and
/// the set forms from the listener or the delta walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Measure {
    Metric(Metric),
    Syscalls,
    Writes,
}

impl Measure {
    pub fn name(self) -> &'static str {
        match self {
            Measure::Metric(m) => m.name(),
            Measure::Syscalls => "syscalls",
            Measure::Writes => "writes",
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Measure::Metric(m) => m.source(),
            Measure::Syscalls => "the seccomp listener",
            Measure::Writes => "the overlay delta",
        }
    }
}

/// A family of units. Each has one base unit that values are stored in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    /// Stored in microseconds.
    Duration,
    /// Stored in bytes.
    Bytes,
    /// A bare whole number.
    Count,
}

impl Unit {
    fn name(self) -> &'static str {
        match self {
            Unit::Duration => "a duration (us, ms or s)",
            Unit::Bytes => "a size (B, KiB, MiB, GiB)",
            Unit::Count => "a whole number",
        }
    }

    /// How many base units one `suffix` is, or `None` if the suffix is not a
    /// unit of this family. The empty suffix is a count's only spelling.
    fn scale(self, suffix: &str) -> Option<f64> {
        Some(match (self, suffix) {
            (Unit::Duration, "us") => 1.0,
            (Unit::Duration, "ms") => 1e3,
            (Unit::Duration, "s") => 1e6,
            (Unit::Bytes, "B") => 1.0,
            (Unit::Bytes, "KiB") => 1024.0,
            (Unit::Bytes, "MiB") => 1024.0 * 1024.0,
            (Unit::Bytes, "GiB") => 1024.0 * 1024.0 * 1024.0,
            (Unit::Bytes, "KB") => 1e3,
            (Unit::Bytes, "MB") => 1e6,
            (Unit::Bytes, "GB") => 1e9,
            (Unit::Count, "") => 1.0,
            _ => return None,
        })
    }

    /// `value` (in this family's base unit) as the report spells it, in the
    /// same style as the resource line under a result.
    pub fn format(self, value: u64) -> String {
        match self {
            Unit::Duration => {
                if value < 1_000 {
                    format!("{value}us")
                } else if value < 1_000_000 {
                    format!("{:.1}ms", value as f64 / 1e3)
                } else {
                    format!("{:.2}s", value as f64 / 1e6)
                }
            }
            Unit::Bytes => crate::output::format_bytes(value),
            Unit::Count => value.to_string(),
        }
    }

    /// `value` as a literal the grammar reads back: the unit that keeps the
    /// number short, no decimals it does not need. `16ms`, `5.4MiB`, `3`.
    pub fn literal(self, value: u64) -> String {
        let (scaled, suffix) = match self {
            Unit::Duration => match value {
                v if v < 1_000 => (v as f64, "us"),
                v if v < 1_000_000 => (v as f64 / 1e3, "ms"),
                v => (v as f64 / 1e6, "s"),
            },
            Unit::Bytes => match value {
                v if v < 1024 => (v as f64, "B"),
                v if v < 1024 * 1024 => (v as f64 / 1024.0, "KiB"),
                v if v < 1024 * 1024 * 1024 => (v as f64 / (1024.0 * 1024.0), "MiB"),
                v => (v as f64 / (1024.0 * 1024.0 * 1024.0), "GiB"),
            },
            Unit::Count => return value.to_string(),
        };
        // Two decimals are as many as a value rounded to two significant
        // figures needs below its unit's next step; the trailing zeros go.
        let mut text = format!("{scaled:.2}");
        if text.contains('.') {
            while text.ends_with('0') {
                text.pop();
            }
            if text.ends_with('.') {
                text.pop();
            }
        }
        text.push_str(suffix);
        text
    }
}

/// Where a number was written: the token `--update-decorators` rewrites.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Literal {
    /// 1-based source line.
    pub line: usize,
    /// Byte range of the number and its unit within that line.
    pub span: Range<usize>,
    /// The token as written, `10ms`: what the file has to still read at
    /// `span` for a rewrite to be safe.
    pub text: String,
}

/// One end of a range.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Bound {
    pub value: u64,
    pub inclusive: bool,
    /// Where the value was written. A chained condition spread over lines
    /// (`2ms <` on one, `cpu < 10ms` on the next) has its two literals on
    /// different lines, which is why this is per bound and not per condition.
    pub literal: Literal,
}

impl Bound {
    /// Does `value` satisfy this bound as the `upper` (or lower) end of a
    /// range?
    pub fn admits(&self, value: u64, upper: bool) -> bool {
        match (upper, self.inclusive) {
            (true, true) => value <= self.value,
            (true, false) => value < self.value,
            (false, true) => value >= self.value,
            (false, false) => value > self.value,
        }
    }
}

/// The range a metric has to fall in, in the metric's base unit. Either end
/// may be open.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Bounds {
    pub lower: Option<Bound>,
    pub upper: Option<Bound>,
}

impl Bounds {
    #[cfg(test)]
    pub fn contains(&self, value: u64) -> bool {
        self.broken_by(value).is_none()
    }

    /// The end of the range `value` falls outside of, if any, with whether it
    /// is the upper one. A value can only be on one side of a non-empty range.
    fn broken_by(&self, value: u64) -> Option<(&Bound, bool)> {
        if let Some(b) = &self.lower
            && !b.admits(value, false)
        {
            return Some((b, false));
        }
        if let Some(b) = &self.upper
            && !b.admits(value, true)
        {
            return Some((b, true));
        }
        None
    }
}

/// What a condition asserts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A metric of the run falls within a range.
    Quantity { metric: Metric, bounds: Bounds },
    /// The set of system calls the test may (`allow`) or may not make. Every
    /// name was checked against this architecture's table at parse time.
    Syscalls { allow: bool, set: Vec<Sysno> },
    /// The hosts the test may contact.
    Hosts(Vec<String>),
    /// The paths the test may write under: each `/absolute` or `./relative`
    /// to the invocation dir, as written.
    Writes(Vec<String>),
}

impl Kind {
    fn metric_name(&self) -> &'static str {
        match self {
            Kind::Quantity { metric, .. } => metric.name(),
            Kind::Syscalls { .. } => "syscalls",
            Kind::Hosts(_) => "hosts",
            Kind::Writes(_) => "writes",
        }
    }

    /// The syscalls a run has to be watched for to check this condition:
    /// the set itself for a deny-list, and everything else for an allow-list.
    /// Empty for a condition that is not about syscalls.
    pub fn watched_syscalls(&self) -> Vec<Sysno> {
        let Kind::Syscalls { allow, set } = self else {
            return Vec::new();
        };
        if *allow {
            Sysno::iter().filter(|nr| !set.contains(nr)).collect()
        } else {
            set.clone()
        }
    }
}

/// How many of a set violation's offenders its message spells out; the rest
/// are counted, and the report lists them all below the snippet.
pub const SHOWN_OFFENDERS: usize = 3;

/// Spell the first [`SHOWN_OFFENDERS`] of `items` and count the rest:
/// `a, b, c (+4 more)`.
fn list_some(items: &[String]) -> String {
    let mut text = items
        .iter()
        .take(SHOWN_OFFENDERS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SHOWN_OFFENDERS {
        text.push_str(&format!(" (+{} more)", items.len() - SHOWN_OFFENDERS));
    }
    text
}

/// Does `path` lie under `root` — or at it?
fn under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// The absolute path a `writes` item stands for: `./x` is `x` under the
/// invocation dir, anything else is already absolute (the parse admits no
/// other spelling).
fn write_root(item: &str, project: &Path) -> PathBuf {
    let item = item.trim_end_matches('/');
    match item.strip_prefix("./") {
        Some(rel) => project.join(rel),
        None if item == "." => project.to_path_buf(),
        None => PathBuf::from(if item.is_empty() { "/" } else { item }),
    }
}

/// One condition of a decorator block, with where it was written: the report
/// points at exactly this condition when it fails, not at the whole block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Condition {
    /// 1-based source line the condition starts on.
    pub line: usize,
    /// Byte range of the condition within that line. A condition that runs on
    /// to the next line is cut at the end of its first one.
    pub span: Range<usize>,
    /// The condition's own text, as written.
    pub text: String,
    pub kind: Kind,
}

/// How a condition came out for one run.
pub enum Verdict {
    Ok,
    Violated(Violation),
    /// The run did not measure what the condition is about, so it was not
    /// checked.
    Unmeasured(Measure),
}

impl Condition {
    pub fn check(&self, sample: &Sample) -> Verdict {
        let violated = |message: String, detail: Detail| {
            Verdict::Violated(Violation {
                line: self.line,
                span: self.span.clone(),
                condition: self.text.clone(),
                message,
                detail,
            })
        };
        match &self.kind {
            Kind::Quantity { metric, bounds } => {
                let Some(value) = metric.measure(sample) else {
                    return Verdict::Unmeasured(Measure::Metric(*metric));
                };
                match bounds.broken_by(value) {
                    None => Verdict::Ok,
                    Some((bound, upper)) => violated(
                        format!("{} was {}", metric.name(), metric.unit().format(value)),
                        Detail::Quantity {
                            metric: *metric,
                            measured: value,
                            broken: Broken {
                                bound: bound.clone(),
                                upper,
                            },
                        },
                    ),
                }
            }
            Kind::Syscalls { allow, set } => {
                let Some(log) = sample.syscalls else {
                    return Verdict::Unmeasured(Measure::Syscalls);
                };
                // The log only ever holds what the run was watching for,
                // which is exactly what this condition forbids — but under a
                // block with several `syscalls` conditions it holds the union
                // of their watch sets, so each one still filters for its own.
                let offenders: Vec<String> = log
                    .counts
                    .keys()
                    .filter(|nr| set.contains(nr) != *allow)
                    .map(|nr| nr.name().to_string())
                    .collect();
                if offenders.is_empty() {
                    return Verdict::Ok;
                }
                let message = if *allow {
                    format!("also made {}", list_some(&offenders))
                } else {
                    format!("made {}", list_some(&offenders))
                };
                violated(message, Detail::Set { offenders })
            }
            Kind::Writes(items) => {
                let Some(writes) = sample.writes else {
                    return Verdict::Unmeasured(Measure::Writes);
                };
                let roots: Vec<PathBuf> = items
                    .iter()
                    .map(|item| write_root(item, sample.project))
                    .collect();
                let offenders: Vec<String> = writes
                    .iter()
                    .filter(|path| !roots.iter().any(|root| under(path, root)))
                    .map(|path| path.display().to_string())
                    .collect();
                if offenders.is_empty() {
                    return Verdict::Ok;
                }
                violated(
                    format!("wrote {}", list_some(&offenders)),
                    Detail::Set { offenders },
                )
            }
            // Not enforced yet; `check_supported` keeps these out of a parse.
            Kind::Hosts(_) => Verdict::Ok,
        }
    }
}

/// A condition that did not hold for a run of its test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Violation {
    /// 1-based source line of the condition.
    pub line: usize,
    /// Byte range of the condition within that line.
    pub span: Range<usize>,
    /// The condition's text, as written.
    pub condition: String,
    /// What was measured instead: `cpu was 12.2ms`, `made socket, connect`,
    /// `wrote /etc/motd`.
    pub message: String,
    pub detail: Detail,
}

/// What a violation has to say beyond its message: the number a quantity
/// condition was broken by, or the members that broke a set condition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Detail {
    Quantity {
        metric: Metric,
        /// The measurement, in the metric's base unit.
        measured: u64,
        /// The end of the range the measurement fell outside of — what
        /// `--update-decorators` rewrites.
        broken: Broken,
    },
    /// Every syscall made or path written that the set did not admit, in
    /// full; the message spells only the first few.
    Set { offenders: Vec<String> },
}

/// The bound a measurement broke, and which end of its range it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Broken {
    pub bound: Bound,
    pub upper: bool,
}

/// One `#attest(...)` block: where it sits in the file and what it asserts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Decorator {
    /// 1-based line of the `#attest(` opener.
    pub first_line: usize,
    /// 1-based line of the closing `)`.
    pub last_line: usize,
    pub conditions: Vec<Condition>,
}

impl Decorator {
    /// Every line of the block, 1-based. These are comments, so the script the
    /// runner builds can carry them at their own line numbers — which is what
    /// lets a violation's snippet render them.
    pub fn lines(&self) -> std::ops::RangeInclusive<usize> {
        self.first_line..=self.last_line
    }

    /// Reject the forms the grammar accepts but no part of a run enforces
    /// yet. An assertion that is written down and never checked is the one
    /// thing a decorator must not be.
    pub fn check_supported(&self) -> Result<(), Error> {
        for c in &self.conditions {
            if matches!(c.kind, Kind::Hosts(_)) {
                return Err(Error {
                    line: c.line,
                    message: format!(
                        "`{}` conditions are not supported yet",
                        c.kind.metric_name()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// A malformed block, and the line it went wrong on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for Error {}

/// Is `line` a comment line (its first non-blank byte is `#`)?
pub fn is_comment_line(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// The text after the `(` of a block opener, if `line` is one, with its byte
/// offset in the line.
fn opener(line: &str) -> Option<usize> {
    let hash = line.find(|c: char| !c.is_ascii_whitespace())?;
    let rest = line[hash..].strip_prefix('#')?;
    // `##` is a doc comment, whatever it goes on to say.
    if rest.starts_with('#') {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("attest")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('(')?;
    Some(line.len() - rest.len())
}

/// A piece of a block's text: `line`, from byte `col` on.
struct Segment<'a> {
    line: usize,
    col: usize,
    text: &'a str,
}

/// Find every `#attest(...)` block in `source`.
///
/// Blocks are returned in source order. Where they attach is the caller's
/// business: this only knows lines, not functions.
pub fn scan(source: &str) -> Result<Vec<Decorator>, Error> {
    let lines: Vec<&str> = source.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(col) = opener(lines[i]) else {
            i += 1;
            continue;
        };
        let first_line = i + 1;
        let mut segments = vec![Segment {
            line: first_line,
            col,
            text: &lines[i][col..],
        }];
        // Gather lines until the `)` that closes the block: the first one
        // outside braces, so a set's `}` can never be mistaken for it (nor,
        // for that matter, a `)` inside one — not that any item has one).
        let mut depth = 0usize;
        let close_at = loop {
            let seg = segments.last().unwrap();
            let closer = seg.text.bytes().position(|b| {
                match b {
                    b'{' => depth += 1,
                    b'}' => depth = depth.saturating_sub(1),
                    b')' if depth == 0 => return true,
                    _ => {}
                }
                false
            });
            if let Some(k) = closer {
                break k;
            }
            let next = seg.line; // 1-based line just consumed == 0-based index of the next
            let Some(line) = lines.get(next) else {
                return Err(Error {
                    line: first_line,
                    message: "`#attest(` is never closed".to_string(),
                });
            };
            // Every continuation line is a comment, and its `#` is not part
            // of the text. A line that is not one is the next thing in the
            // file — code, a blank, a function — and the block is unfinished.
            let Some(hash) = line.find('#').filter(|_| is_comment_line(line)) else {
                return Err(Error {
                    line: first_line,
                    message: "`#attest(` is not closed before the end of its comment block"
                        .to_string(),
                });
            };
            segments.push(Segment {
                line: next + 1,
                col: hash + 1,
                text: &line[hash + 1..],
            });
        };
        let last = segments.last_mut().unwrap();
        let last_line = last.line;
        if !last.text[close_at + 1..].trim().is_empty() {
            return Err(Error {
                line: last_line,
                message: "unexpected text after the `)` that closes `#attest(`".to_string(),
            });
        }
        last.text = &last.text[..close_at];

        let conditions = parse_conditions(&segments)?;
        blocks.push(Decorator {
            first_line,
            last_line,
            conditions,
        });
        i = last_line;
    }
    Ok(blocks)
}

/// Split the text of a block into its conditions and parse each one.
fn parse_conditions(segments: &[Segment]) -> Result<Vec<Condition>, Error> {
    // The segments are joined into one text, with a map back from an offset
    // in it to the line and column it came from: that is what a condition's
    // span is, and what an error names.
    let mut text = String::new();
    let mut starts = Vec::with_capacity(segments.len());
    for (n, seg) in segments.iter().enumerate() {
        if n > 0 {
            text.push('\n');
        }
        starts.push((text.len(), seg.line, seg.col));
        text.push_str(seg.text);
    }
    let locate = |offset: usize| -> (usize, usize) {
        let &(start, line, col) = starts
            .iter()
            .rev()
            .find(|&&(start, _, _)| start <= offset)
            .unwrap();
        (line, col + (offset - start))
    };
    let first_line = segments[0].line;

    // Split at the commas outside braces.
    let mut pieces: Vec<Range<usize>> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (k, b) in text.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                pieces.push(start..k);
                start = k + 1;
            }
            _ => {}
        }
    }
    pieces.push(start..text.len());

    // A block with nothing in it at all is a mistake of its own.
    if text.trim().is_empty() {
        return Err(Error {
            line: first_line,
            message: "`#attest()` asserts nothing".to_string(),
        });
    }
    // A trailing comma leaves an empty last piece, which is allowed; an empty
    // piece anywhere else (`,,`, a leading comma) is not.
    if pieces.len() > 1 && text[pieces.last().unwrap().clone()].trim().is_empty() {
        pieces.pop();
    }
    let mut conditions = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let raw = &text[piece.clone()];
        let trimmed = raw.trim();
        let begin = piece.start + (raw.len() - raw.trim_start().len());
        let (line, col) = locate(begin);
        if trimmed.is_empty() {
            return Err(Error {
                line,
                message: "empty condition".to_string(),
            });
        }
        let mut kind = parse_condition(trimmed).map_err(|message| Error { line, message })?;
        // A quantity's literals were located as offsets into the condition's
        // text; put them on the lines of the file.
        if let Kind::Quantity { bounds, .. } = &mut kind {
            for bound in [&mut bounds.lower, &mut bounds.upper].into_iter().flatten() {
                let lit = &mut bound.literal;
                let (lit_line, lit_col) = locate(begin + lit.span.start);
                lit.span = lit_col..lit_col + lit.span.len();
                lit.line = lit_line;
            }
        }
        // The span is cut at the end of the condition's first line: that is
        // the one line a snippet highlights.
        let first_line_len = trimmed.split('\n').next().unwrap().trim_end().len();
        conditions.push(Condition {
            line,
            span: col..col + first_line_len,
            text: trimmed.to_string(),
            kind,
        });
    }
    Ok(conditions)
}

/// Parse one condition's text.
fn parse_condition(text: &str) -> Result<Kind, String> {
    // The set forms first: their items are raw text (host names, paths), not
    // tokens, so they are read by shape rather than lexed.
    if let Some(rest) = strip_word(text, "syscalls") {
        let (allow, rest) = if let Some(rest) = strip_word(rest, "not") {
            (false, rest)
        } else {
            (true, rest)
        };
        let rest = strip_word(rest, "in")
            .ok_or_else(|| "expected `in` or `not in` after `syscalls`".to_string())?;
        let set = parse_set(rest)?
            .iter()
            .map(|name| {
                name.parse::<Sysno>().map_err(|_| {
                    format!(
                        "`{name}` is not a system call on {}",
                        std::env::consts::ARCH
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Kind::Syscalls { allow, set });
    }
    if let Some(rest) = strip_word(text, "hosts") {
        let rest =
            strip_word(rest, "in").ok_or_else(|| "expected `in` after `hosts`".to_string())?;
        return Ok(Kind::Hosts(parse_set(rest)?));
    }
    if let Some(rest) = strip_word(text, "writes") {
        let rest =
            strip_word(rest, "in").ok_or_else(|| "expected `in` after `writes`".to_string())?;
        let items = parse_set(rest)?;
        // A path is either absolute or explicitly relative to the project:
        // a bare `build` would read as one or the other depending on who is
        // asked, and `../x` would name something outside the tree the test
        // file is in.
        for item in &items {
            if !(item.starts_with('/') || item == "." || item.starts_with("./")) {
                return Err(format!(
                    "`{item}` must be an absolute path or start with `./` (relative to the invocation dir)"
                ));
            }
            if Path::new(item)
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                return Err(format!("`{item}` must not contain `..`"));
            }
        }
        return Ok(Kind::Writes(items));
    }
    parse_quantity_condition(text)
}

/// `text` without a leading word `word`, if that is how it starts. The word
/// must end there: `hostsx` does not start with the word `hosts`.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(word)?;
    if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(rest.trim_start())
}

/// Parse `{a, b, c}`. Items are trimmed raw text; a trailing comma is allowed
/// and an empty item is not.
fn parse_set(text: &str) -> Result<Vec<String>, String> {
    let inner = text
        .strip_prefix('{')
        .ok_or_else(|| "expected `{` to open a set".to_string())?;
    let inner = inner
        .strip_suffix('}')
        .ok_or_else(|| "expected `}` to close the set".to_string())?;
    if inner.contains('{') || inner.contains('}') {
        return Err("a set cannot contain another set".to_string());
    }
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut items: Vec<&str> = inner.split(',').map(str::trim).collect();
    if items.len() > 1 && items.last().unwrap().is_empty() {
        items.pop();
    }
    if items.iter().any(|item| item.is_empty()) {
        return Err("empty item in set".to_string());
    }
    Ok(items.into_iter().map(str::to_string).collect())
}

/// One side of a comparison.
enum Operand {
    Metric(Metric),
    /// A number with the unit suffix it was written with, not yet scaled:
    /// which family it must be in is only known once the metric is. `span`
    /// is where the token sits in the condition's text.
    Quantity {
        number: f64,
        suffix: String,
        span: Range<usize>,
        text: String,
    },
}

/// Which way a comparison points. `Less` is `<`/`<=`: left is below right.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Less,
    Greater,
}

struct Op {
    direction: Direction,
    inclusive: bool,
}

/// Parse `METRIC OP QTY`, `QTY OP METRIC` or `QTY OP METRIC OP QTY`. The
/// literals' spans are offsets into `text`.
fn parse_quantity_condition(text: &str) -> Result<Kind, String> {
    let mut operands = Vec::new();
    let mut ops = Vec::new();
    let mut rest = text;
    loop {
        let at = text.len() - rest.len();
        let (operand, after) = parse_operand(rest, at)?;
        operands.push(operand);
        rest = after.trim_start();
        if rest.is_empty() {
            break;
        }
        let (op, after) = parse_op(rest)?;
        ops.push(op);
        rest = after.trim_start();
        if rest.is_empty() {
            return Err("expected a value after the comparison".to_string());
        }
    }

    // The shapes a condition can take: exactly one metric, compared with one
    // or two quantities.
    let metric_at = operands
        .iter()
        .position(|o| matches!(o, Operand::Metric(_)))
        .ok_or_else(|| "a condition compares a metric (cpu, wall, mem, io_read, io_write, pids) against a value".to_string())?;
    let metric = match operands[metric_at] {
        Operand::Metric(m) => m,
        _ => unreachable!(),
    };
    if operands
        .iter()
        .filter(|o| matches!(o, Operand::Metric(_)))
        .count()
        > 1
    {
        return Err("a condition compares only one metric".to_string());
    }
    let valid_shape = match operands.len() {
        2 => true,
        3 => metric_at == 1,
        _ => false,
    };
    if !valid_shape {
        return Err(
            "expected `cpu < 10ms`, `2ms < cpu` or `2ms < cpu < 10ms` (one metric, at most two values)"
                .to_string(),
        );
    }
    if ops.len() == 2 && ops[0].direction != ops[1].direction {
        return Err(
            "a chained comparison must point the same way, as in `2ms < cpu < 10ms`".to_string(),
        );
    }

    let unit = metric.unit();
    let mut bounds = Bounds::default();
    for (k, op) in ops.iter().enumerate() {
        // Each comparison has the metric on one side and a quantity on the
        // other; which side the quantity is on and which way the operator
        // points together say which end of the range it is.
        let (quantity, metric_is_left) = match (&operands[k], &operands[k + 1]) {
            (Operand::Metric(_), q) => (q, true),
            (q, Operand::Metric(_)) => (q, false),
            _ => unreachable!("the shape check leaves a metric in every pair"),
        };
        let Operand::Quantity {
            number,
            suffix,
            span,
            text,
        } = quantity
        else {
            unreachable!()
        };
        let value = scale(*number, suffix, unit)?;
        let bound = Bound {
            value,
            inclusive: op.inclusive,
            literal: Literal {
                line: 0,
                span: span.clone(),
                text: text.clone(),
            },
        };
        let is_upper = (op.direction == Direction::Less) == metric_is_left;
        let slot = if is_upper {
            &mut bounds.upper
        } else {
            &mut bounds.lower
        };
        if slot.is_some() {
            return Err("both comparisons bound the same end of the range".to_string());
        }
        *slot = Some(bound);
    }
    if let (Some(lo), Some(hi)) = (&bounds.lower, &bounds.upper)
        && (lo.value > hi.value || (lo.value == hi.value && !(lo.inclusive && hi.inclusive)))
    {
        return Err("the range is empty: its lower bound is not below its upper bound".to_string());
    }
    Ok(Kind::Quantity { metric, bounds })
}

/// `number` written with `suffix`, in the base unit of `unit`.
fn scale(number: f64, suffix: &str, unit: Unit) -> Result<u64, String> {
    let factor = unit.scale(suffix).ok_or_else(|| {
        if suffix.is_empty() {
            format!("a value for this metric needs a unit: {}", unit.name())
        } else {
            format!(
                "`{suffix}` is not a unit for this metric, which takes {}",
                unit.name()
            )
        }
    })?;
    let scaled = number * factor;
    if unit == Unit::Count && scaled.fract() != 0.0 {
        return Err("a count is a whole number".to_string());
    }
    Ok(scaled.round() as u64)
}

/// Read a metric name or a number with its unit suffix off the front of `text`,
/// which starts `at` bytes into the condition.
fn parse_operand(text: &str, at: usize) -> Result<(Operand, &str), String> {
    let at = at + (text.len() - text.trim_start().len());
    let text = text.trim_start();
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(text.len());
    let word = &text[..end];
    if word.is_empty() {
        return Err(if text.is_empty() {
            "expected a metric or a value".to_string()
        } else {
            format!("unexpected `{}`", text.chars().next().unwrap())
        });
    }
    let rest = &text[end..];
    if word.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        let digits = word
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(word.len());
        let (num, suffix) = word.split_at(digits);
        let number: f64 = num
            .parse()
            .map_err(|_| format!("`{num}` is not a number"))?;
        return Ok((
            Operand::Quantity {
                number,
                suffix: suffix.to_string(),
                span: at..at + end,
                text: word.to_string(),
            },
            rest,
        ));
    }
    match Metric::parse(word) {
        Some(metric) => Ok((Operand::Metric(metric), rest)),
        None => Err(format!(
            "`{word}` is not a metric (cpu, wall, mem, io_read, io_write, pids)"
        )),
    }
}

fn parse_op(text: &str) -> Result<(Op, &str), String> {
    let (op, len) = if text.starts_with("<=") {
        (
            Op {
                direction: Direction::Less,
                inclusive: true,
            },
            2,
        )
    } else if text.starts_with(">=") {
        (
            Op {
                direction: Direction::Greater,
                inclusive: true,
            },
            2,
        )
    } else if text.starts_with('<') {
        (
            Op {
                direction: Direction::Less,
                inclusive: false,
            },
            1,
        )
    } else if text.starts_with('>') {
        (
            Op {
                direction: Direction::Greater,
                inclusive: false,
            },
            1,
        )
    } else {
        let shown: String = text.chars().take(8).collect();
        return Err(format!("expected `<`, `<=`, `>` or `>=` at `{shown}`"));
    };
    Ok((op, &text[len..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(source: &str) -> Decorator {
        let mut blocks = scan(source).unwrap();
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        blocks.pop().unwrap()
    }

    /// A sample with the quantities given and nothing watched.
    fn sample_of(wall: Duration, resources: Option<&ResourceStats>) -> Sample<'_> {
        Sample {
            wall,
            resources,
            syscalls: None,
            writes: None,
            project: Path::new("/proj"),
        }
    }

    /// A range as `(lower, upper)`, each `(value, inclusive)`: what the
    /// grammar tests care about, without the literals' positions.
    type Ends = (Option<(u64, bool)>, Option<(u64, bool)>);

    fn ends(b: &Bounds) -> Ends {
        let end = |b: &Option<Bound>| b.as_ref().map(|b| (b.value, b.inclusive));
        (end(&b.lower), end(&b.upper))
    }

    fn quantity(cond: &Condition) -> (Metric, Bounds) {
        match &cond.kind {
            Kind::Quantity { metric, bounds } => (*metric, bounds.clone()),
            other => panic!("not a quantity condition: {other:?}"),
        }
    }

    fn bound(value: u64, inclusive: bool) -> Option<(u64, bool)> {
        Some((value, inclusive))
    }

    fn make_bounds(lower: Option<(u64, bool)>, upper: Option<(u64, bool)>) -> Bounds {
        let mk = |e: Option<(u64, bool)>| {
            e.map(|(value, inclusive)| Bound {
                value,
                inclusive,
                literal: Literal::default(),
            })
        };
        Bounds {
            lower: mk(lower),
            upper: mk(upper),
        }
    }

    #[test]
    fn single_condition() {
        let d = one("#attest(cpu < 10ms)\ntestX() {\n  true\n}\n");
        assert_eq!((d.first_line, d.last_line), (1, 1));
        assert_eq!(d.conditions.len(), 1);
        let c = &d.conditions[0];
        assert_eq!(c.line, 1);
        assert_eq!(&"#attest(cpu < 10ms)"[c.span.clone()], "cpu < 10ms");
        assert_eq!(c.text, "cpu < 10ms");
        let (metric, bounds) = quantity(c);
        assert_eq!(metric, Metric::Cpu);
        assert_eq!(ends(&bounds), (None, bound(10_000, false)));
        let lit = &bounds.upper.unwrap().literal;
        assert_eq!(lit.line, 1);
        assert_eq!(&"#attest(cpu < 10ms)"[lit.span.clone()], "10ms");
        assert_eq!(lit.text, "10ms");
    }

    #[test]
    fn every_shape_of_a_range() {
        let cases = [
            ("cpu < 10ms", None, bound(10_000, false)),
            ("cpu <= 10ms", None, bound(10_000, true)),
            ("cpu > 2ms", bound(2_000, false), None),
            ("cpu >= 2ms", bound(2_000, true), None),
            ("2ms < cpu", bound(2_000, false), None),
            ("10ms > cpu", None, bound(10_000, false)),
            ("10ms >= cpu", None, bound(10_000, true)),
            (
                "2ms < cpu < 10ms",
                bound(2_000, false),
                bound(10_000, false),
            ),
            (
                "2ms <= cpu <= 10ms",
                bound(2_000, true),
                bound(10_000, true),
            ),
            (
                "10ms > cpu > 2ms",
                bound(2_000, false),
                bound(10_000, false),
            ),
            (
                "10ms >= cpu >= 2ms",
                bound(2_000, true),
                bound(10_000, true),
            ),
            // Whitespace is free.
            ("2ms<cpu<10ms", bound(2_000, false), bound(10_000, false)),
            (
                "  2ms   <   cpu   <   10ms  ",
                bound(2_000, false),
                bound(10_000, false),
            ),
        ];
        for (text, lower, upper) in cases {
            let d = one(&format!("#attest({text})\n"));
            let (metric, bounds) = quantity(&d.conditions[0]);
            assert_eq!(metric, Metric::Cpu, "{text}");
            assert_eq!(ends(&bounds), (lower, upper), "{text}");
        }
    }

    #[test]
    fn units_belong_to_their_metric() {
        let ok = [
            ("cpu < 1.5ms", Metric::Cpu, 1_500),
            ("cpu < 250us", Metric::Cpu, 250),
            ("wall < 2s", Metric::Wall, 2_000_000),
            ("wall < 0.5s", Metric::Wall, 500_000),
            ("mem < 50MiB", Metric::Mem, 50 * 1024 * 1024),
            ("mem < 1.5KiB", Metric::Mem, 1536),
            ("mem < 1GiB", Metric::Mem, 1 << 30),
            ("mem < 2MB", Metric::Mem, 2_000_000),
            ("io_read < 100B", Metric::IoRead, 100),
            ("io_write < 4KB", Metric::IoWrite, 4_000),
            ("pids <= 3", Metric::Pids, 3),
        ];
        for (text, want_metric, want_value) in ok {
            let d = one(&format!("#attest({text})\n"));
            let (metric, bounds) = quantity(&d.conditions[0]);
            assert_eq!(metric, want_metric, "{text}");
            assert_eq!(bounds.upper.unwrap().value, want_value, "{text}");
        }

        for text in [
            "cpu < 10",    // a duration needs a unit
            "cpu < 10MiB", // wrong family
            "mem < 10ms",  // wrong family
            "mem < 10",    // a size needs a unit
            "pids < 3ms",  // a count takes no unit
            "pids < 1.5",  // and is whole
            "cpu < 10 ms", // the unit is attached to the number
        ] {
            assert!(scan(&format!("#attest({text})\n")).is_err(), "{text}");
        }
    }

    #[test]
    fn malformed_ranges_are_errors() {
        for text in [
            "2ms < cpu > 10ms",  // chain points both ways
            "2ms > cpu < 10ms",  // likewise
            "10ms < cpu < 2ms",  // empty range
            "2ms < cpu < 2ms",   // empty range
            "2ms < cpu <= 2ms",  // empty range
            "cpu < 2ms < 10ms",  // metric must be in the middle of a chain
            "2ms < 10ms",        // no metric
            "cpu < wall",        // two metrics
            "cpu",               // no comparison
            "cpu <",             // no value
            "< 10ms",            // no left side
            "cpu == 10ms",       // not an operator
            "cpu < 10ms < 20ms", // metric not in the middle
            "load < 1",          // no such metric
            "cpu < 1ms extra",   // trailing junk
        ] {
            assert!(scan(&format!("#attest({text})\n")).is_err(), "{text}");
        }
        // The empty range at equal bounds is fine when both are inclusive.
        let d = one("#attest(2ms <= cpu <= 2ms)\n");
        let (_, bounds) = quantity(&d.conditions[0]);
        assert!(bounds.contains(2_000));
        assert!(!bounds.contains(1_999));
    }

    #[test]
    fn bounds_contain() {
        let b = make_bounds(bound(2, false), bound(10, true));
        assert!(!b.contains(2));
        assert!(b.contains(3));
        assert!(b.contains(10));
        assert!(!b.contains(11));
        assert!(Bounds::default().contains(u64::MAX));
    }

    #[test]
    fn several_conditions_in_one_block() {
        let line = "#attest(cpu < 10ms, pids <= 3, wall < 1s)";
        let d = one(line);
        let texts: Vec<&str> = d.conditions.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["cpu < 10ms", "pids <= 3", "wall < 1s"]);
        for c in &d.conditions {
            assert_eq!(c.line, 1);
            assert_eq!(&line[c.span.clone()], c.text, "span of {}", c.text);
        }
    }

    #[test]
    fn trailing_comma_is_allowed_but_not_required() {
        assert_eq!(one("#attest(cpu < 10ms,)").conditions.len(), 1);
        assert_eq!(one("#attest(cpu < 10ms, pids < 2,)").conditions.len(), 2);
        assert_eq!(one("#attest(cpu < 10ms, pids < 2)").conditions.len(), 2);
        // But a comma has to separate two things.
        for text in [
            "#attest(,)",
            "#attest(cpu < 1ms,,)",
            "#attest(, cpu < 1ms)",
            "#attest(cpu < 1ms,, pids < 2)",
        ] {
            assert!(scan(text).is_err(), "{text}");
        }
        assert!(scan("#attest()").is_err());
        assert!(scan("#attest(  )").is_err());
    }

    #[test]
    fn multi_line_block() {
        let source = "\
#!/bin/sh

# attest(
#   2ms < cpu < 10ms,
#   pids <= 3,
# )
testX() {
  true
}
";
        let d = one(source);
        assert_eq!((d.first_line, d.last_line), (3, 6));
        assert_eq!(d.lines().collect::<Vec<_>>(), [3, 4, 5, 6]);
        assert_eq!(d.conditions.len(), 2);
        let lines: Vec<&str> = source.lines().collect();
        let c = &d.conditions[0];
        assert_eq!(c.line, 4);
        assert_eq!(&lines[3][c.span.clone()], "2ms < cpu < 10ms");
        let c = &d.conditions[1];
        assert_eq!(c.line, 5);
        assert_eq!(&lines[4][c.span.clone()], "pids <= 3");
    }

    #[test]
    fn multi_line_block_without_indentation_and_without_trailing_comma() {
        let d = one("#attest(\n#cpu < 10ms,\n#pids <= 3\n#)\n");
        assert_eq!((d.first_line, d.last_line), (1, 4));
        assert_eq!(d.conditions.len(), 2);
        assert_eq!(d.conditions[1].line, 3);
        // The closing `)` can share a line with the last condition.
        let d = one("#attest(\n#  cpu < 10ms)\n");
        assert_eq!((d.first_line, d.last_line), (1, 2));
        // And the first condition can share the opener's line.
        let d = one("#attest(cpu < 10ms,\n#  pids <= 3)\n");
        assert_eq!(d.conditions.len(), 2);
        assert_eq!(d.conditions[1].line, 2);
    }

    #[test]
    fn a_condition_spanning_lines_is_highlighted_on_its_first() {
        let d = one("#attest(\n#  2ms <\n#  cpu < 10ms\n#)\n");
        let c = &d.conditions[0];
        assert_eq!(c.line, 2);
        assert_eq!(&"#  2ms <"[c.span.clone()], "2ms <");
        let (metric, bounds) = quantity(c);
        assert_eq!(metric, Metric::Cpu);
        assert_eq!(ends(&bounds), (bound(2_000, false), bound(10_000, false)));
        // The two literals are on different lines, and each knows its own.
        let lo = bounds.lower.unwrap().literal;
        let hi = bounds.upper.unwrap().literal;
        assert_eq!((lo.line, &"#  2ms <"[lo.span.clone()]), (2, "2ms"));
        assert_eq!((hi.line, &"#  cpu < 10ms"[hi.span.clone()]), (3, "10ms"));
    }

    #[test]
    fn blanks_around_attest_are_allowed() {
        for opener in [
            "#attest(",
            "# attest(",
            "#  attest (",
            "#\tattest\t(",
            "   # attest(",
        ] {
            let d = one(&format!("{opener}cpu < 10ms)\n"));
            assert_eq!(d.conditions.len(), 1, "{opener}");
        }
    }

    #[test]
    fn a_doc_comment_is_not_a_decorator() {
        // `##` is how a test's description is written; one that mentions
        // attest must stay prose.
        for source in [
            "## attest(cpu < 1ms)\ntestX() {\n  true\n}\n",
            "## see attest(1) for details\n",
            "# attestation(cpu < 1ms)\n",
            "# attest\n",
            "# run attest(cpu < 1ms)\n",
            "echo '# attest(' # not a comment line\n",
        ] {
            assert!(scan(source).unwrap().is_empty(), "{source:?}");
        }
    }

    #[test]
    fn an_unclosed_block_is_an_error() {
        // The block is cut off by code, by a blank line, by a function, and by
        // the end of the file.
        for source in [
            "#attest(cpu < 10ms\ntrue\n",
            "#attest(cpu < 10ms\n\n#)\n",
            "#attest(cpu < 10ms\ntestX() {\n  true\n}\n",
            "#attest(cpu < 10ms\n",
            "#attest(\n#  cpu < 10ms,\n",
        ] {
            let err = scan(source).unwrap_err();
            assert_eq!(err.line, 1, "{source:?}: {err}");
        }
        // The bare-line spelling of a multi-line block is the mistake this
        // catches: without the `#` those lines are shell, not comment.
        assert!(scan("#attest(\n  cpu > 0ms,\n)\n").is_err());
        // Text after the `)` is not the shell's, and not the block's either.
        assert_eq!(
            scan("#attest(cpu < 10ms) testX() {\n}\n").unwrap_err().line,
            1
        );
        assert_eq!(scan("#attest(\n# cpu < 10ms) x\n").unwrap_err().line, 2);
    }

    #[test]
    fn errors_name_the_line_of_the_condition() {
        let err = scan("#attest(\n#  cpu < 10ms,\n#  load < 3,\n#)\n").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("load"), "{err}");
    }

    #[test]
    fn several_blocks_in_one_file() {
        let blocks = scan("#attest(cpu < 10ms)\ntestA() {\n  true\n}\n\n# attest(pids < 2)\ntestB() {\n  true\n}\n").unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].first_line, 1);
        assert_eq!(blocks[1].first_line, 6);
    }

    #[test]
    fn set_conditions_parse() {
        let d = one(
            "#attest(syscalls not in {socket, connect}, hosts in {}, writes in {./build, /tmp,})",
        );
        assert_eq!(
            d.conditions[0].kind,
            Kind::Syscalls {
                allow: false,
                set: vec![Sysno::socket, Sysno::connect]
            }
        );
        assert_eq!(d.conditions[1].kind, Kind::Hosts(vec![]));
        assert_eq!(
            d.conditions[2].kind,
            Kind::Writes(vec!["./build".into(), "/tmp".into()])
        );
        let d = one("#attest(syscalls in {read, write})");
        assert_eq!(
            d.conditions[0].kind,
            Kind::Syscalls {
                allow: true,
                set: vec![Sysno::read, Sysno::write]
            }
        );
        // A syscall is a name in this architecture's table, and a write
        // root is a path that says where it is relative to.
        for (text, word) in [
            ("syscalls in {read, frobnicate}", "frobnicate"),
            ("writes in {build}", "build"),
            ("writes in {../x}", "../x"),
            ("writes in {./a/../b}", "`..`"),
        ] {
            let err = scan(&format!("#attest({text})\n")).unwrap_err();
            assert!(err.message.contains(word), "{text}: {err}");
        }
        let d = one("#attest(writes in {., ./, /, /tmp/})");
        assert_eq!(
            d.conditions[0].kind,
            Kind::Writes(vec![".".into(), "./".into(), "/".into(), "/tmp/".into()])
        );
        // Items may hold what a host or a path holds.
        let d = one("#attest(hosts in {api.example.com, 10.0.0.0/8, [::1]:443})");
        assert_eq!(
            d.conditions[0].kind,
            Kind::Hosts(vec![
                "api.example.com".into(),
                "10.0.0.0/8".into(),
                "[::1]:443".into()
            ])
        );
        // A set may be spread over lines, and its commas do not split
        // conditions.
        let d = one(
            "#attest(\n#  hosts in {\n#    a.example,\n#    b.example,\n#  },\n#  cpu < 1s,\n#)",
        );
        assert_eq!(d.conditions.len(), 2);
        assert_eq!(d.conditions[0].line, 2);
        assert_eq!(d.conditions[1].line, 6);

        for text in [
            "hosts in",
            "hosts in {",
            "hosts in a.example",
            "hosts {a}",
            "hosts not in {a}",
            "syscalls in {a,,b}",
            "syscalls in {,}",
            "syscalls in {{a}}",
            "hostsx in {}",
            "writes in {} extra",
        ] {
            assert!(scan(&format!("#attest({text})\n")).is_err(), "{text}");
        }
    }

    #[test]
    fn unenforced_forms_are_refused_by_check_supported() {
        assert!(
            one("#attest(cpu < 1ms, pids < 2)")
                .check_supported()
                .is_ok()
        );
        for text in ["syscalls in {}", "writes in {}"] {
            let d = one(&format!("#attest(cpu < 1ms,\n# {text})"));
            assert!(d.check_supported().is_ok(), "{text}");
        }
        let d = one("#attest(cpu < 1ms,\n# hosts in {})");
        let err = d.check_supported().unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("not supported yet"), "{err}");
    }

    #[test]
    fn syscalls_condition_is_checked_against_the_log() {
        let mut log = SyscallLog::default();
        for nr in [Sysno::read, Sysno::read, Sysno::socket, Sysno::connect] {
            log.record(nr);
        }
        let mut s = sample_of(Duration::ZERO, None);
        s.syscalls = Some(&log);

        let d = one(
            "#attest(syscalls not in {socket, connect}, syscalls not in {execve}, syscalls in {read, write})",
        );
        let Verdict::Violated(v) = d.conditions[0].check(&s) else {
            panic!("expected a violation");
        };
        assert_eq!(v.message, "made socket, connect");
        assert_eq!(
            v.detail,
            Detail::Set {
                offenders: vec!["socket".into(), "connect".into()]
            }
        );
        assert!(matches!(d.conditions[1].check(&s), Verdict::Ok));
        let Verdict::Violated(v) = d.conditions[2].check(&s) else {
            panic!("expected a violation");
        };
        assert_eq!(v.message, "also made socket, connect");

        // Nothing watched: nothing to say.
        assert!(matches!(
            d.conditions[0].check(&sample_of(Duration::ZERO, None)),
            Verdict::Unmeasured(Measure::Syscalls)
        ));
    }

    #[test]
    fn watched_syscalls_are_the_set_or_its_complement() {
        let d = one("#attest(syscalls not in {socket}, syscalls in {read}, cpu < 1s)");
        assert_eq!(d.conditions[0].kind.watched_syscalls(), vec![Sysno::socket]);
        let watched = d.conditions[1].kind.watched_syscalls();
        assert!(!watched.contains(&Sysno::read));
        assert!(watched.contains(&Sysno::socket));
        assert!(d.conditions[2].kind.watched_syscalls().is_empty());
    }

    #[test]
    fn writes_condition_is_checked_against_the_delta() {
        let writes = [
            PathBuf::from("/proj/build/out"),
            PathBuf::from("/tmp/scratch"),
            PathBuf::from("/etc/motd"),
            PathBuf::from("/proj/src/x"),
        ];
        let mut s = sample_of(Duration::ZERO, None);
        s.writes = Some(&writes);

        let d = one("#attest(writes in {./build, /tmp/}, writes in {/, ./}, writes in {.})");
        let Verdict::Violated(v) = d.conditions[0].check(&s) else {
            panic!("expected a violation");
        };
        assert_eq!(v.message, "wrote /etc/motd, /proj/src/x");
        assert!(matches!(d.conditions[1].check(&s), Verdict::Ok));
        let Verdict::Violated(v) = d.conditions[2].check(&s) else {
            panic!("expected a violation");
        };
        assert_eq!(v.message, "wrote /tmp/scratch, /etc/motd");

        // A prefix is a path prefix, not a string prefix.
        let writes = [PathBuf::from("/tmpfoo")];
        s.writes = Some(&writes);
        assert!(matches!(d.conditions[0].check(&s), Verdict::Violated(_)));

        assert!(matches!(
            d.conditions[0].check(&sample_of(Duration::ZERO, None)),
            Verdict::Unmeasured(Measure::Writes)
        ));
    }

    #[test]
    fn a_long_list_of_offenders_is_cut_short() {
        let items: Vec<String> = (1..=5).map(|i| format!("p{i}")).collect();
        assert_eq!(list_some(&items), "p1, p2, p3 (+2 more)");
        assert_eq!(list_some(&items[..3]), "p1, p2, p3");
    }

    #[test]
    fn check_measures_each_metric_from_its_source() {
        let stats = ResourceStats {
            cpu_user_usec: Some(8_000),
            cpu_system_usec: Some(4_000),
            memory_peak: Some(3 * 1024 * 1024),
            io_read_bytes: Some(10),
            io_write_bytes: Some(20),
            pids_peak: Some(5),
        };
        let sample = sample_of(Duration::from_millis(1500), Some(&stats));
        let d = one(
            "#attest(cpu < 10ms, wall < 1s, mem < 4MiB, io_read < 5B, io_write < 25B, pids <= 5)",
        );
        let verdicts: Vec<Option<String>> = d
            .conditions
            .iter()
            .map(|c| match c.check(&sample) {
                Verdict::Ok => None,
                Verdict::Violated(v) => Some(v.message),
                Verdict::Unmeasured(m) => panic!("{} unmeasured", m.name()),
            })
            .collect();
        assert_eq!(
            verdicts,
            [
                Some("cpu was 12.0ms".to_string()),
                Some("wall was 1.50s".to_string()),
                None,
                Some("io_read was 10B".to_string()),
                None,
                None,
            ]
        );
    }

    #[test]
    fn check_reports_what_it_could_not_measure() {
        let sample = sample_of(Duration::from_millis(1), None);
        let d = one("#attest(cpu < 10ms, wall < 1s)");
        assert!(matches!(
            d.conditions[0].check(&sample),
            Verdict::Unmeasured(Measure::Metric(Metric::Cpu))
        ));
        assert!(matches!(d.conditions[1].check(&sample), Verdict::Ok));

        // A cgroup without the cpu controller.
        let stats = ResourceStats {
            pids_peak: Some(1),
            ..Default::default()
        };
        let sample = sample_of(Duration::from_millis(1), Some(&stats));
        assert!(matches!(
            d.conditions[0].check(&sample),
            Verdict::Unmeasured(Measure::Metric(Metric::Cpu))
        ));
    }

    #[test]
    fn violation_carries_the_condition_and_its_place() {
        let stats = ResourceStats {
            cpu_user_usec: Some(100),
            cpu_system_usec: Some(50),
            ..Default::default()
        };
        let sample = sample_of(Duration::ZERO, Some(&stats));
        let d = one("#attest(\n#  1ms < cpu < 2ms,\n#)");
        let Verdict::Violated(v) = d.conditions[0].check(&sample) else {
            panic!("expected a violation");
        };
        assert_eq!(v.line, 2);
        assert_eq!(&"#  1ms < cpu < 2ms,"[v.span.clone()], "1ms < cpu < 2ms");
        assert_eq!(v.condition, "1ms < cpu < 2ms");
        assert_eq!(v.message, "cpu was 150us");
    }

    #[test]
    fn violation_names_the_bound_it_broke() {
        let stats = ResourceStats {
            cpu_user_usec: Some(100),
            cpu_system_usec: Some(50),
            pids_peak: Some(9),
            ..Default::default()
        };
        let sample = sample_of(Duration::ZERO, Some(&stats));
        let line = "#attest(1ms < cpu < 2ms, pids <= 3)";
        let d = one(line);
        let Verdict::Violated(v) = d.conditions[0].check(&sample) else {
            panic!("expected a violation");
        };
        let Detail::Quantity {
            metric,
            measured,
            broken,
        } = v.detail
        else {
            panic!("expected a quantity");
        };
        assert_eq!((metric, measured), (Metric::Cpu, 150));
        // 150us is below the lower bound, so that is the one to rewrite.
        assert!(!broken.upper);
        assert_eq!(broken.bound.value, 1_000);
        assert_eq!(&line[broken.bound.literal.span.clone()], "1ms");

        let Verdict::Violated(v) = d.conditions[1].check(&sample) else {
            panic!("expected a violation");
        };
        let Detail::Quantity { broken, .. } = v.detail else {
            panic!("expected a quantity");
        };
        assert!(broken.upper);
        assert_eq!(broken.bound.literal.line, 1);
        assert_eq!(&line[broken.bound.literal.span.clone()], "3");
    }

    #[test]
    fn unit_literal_reads_back_as_written() {
        let cases = [
            (Unit::Duration, 16_000, "16ms"),
            (Unit::Duration, 999, "999us"),
            (Unit::Duration, 1_500_000, "1.5s"),
            (Unit::Duration, 12_500, "12.5ms"),
            (Unit::Bytes, 512, "512B"),
            (Unit::Bytes, 5_662_310, "5.4MiB"),
            (Unit::Bytes, 20 * 1024 * 1024, "20MiB"),
            (Unit::Bytes, 3 * 1024 * 1024 * 1024, "3GiB"),
            (Unit::Count, 7, "7"),
        ];
        for (unit, value, want) in cases {
            let lit = unit.literal(value);
            assert_eq!(lit, want);
            // What is written is what the grammar reads back, with the value
            // it was written from (to the precision the literal carries).
            let metric = match unit {
                Unit::Duration => "cpu",
                Unit::Bytes => "mem",
                Unit::Count => "pids",
            };
            let d = one(&format!("#attest({metric} <= {lit})"));
            let (_, bounds) = quantity(&d.conditions[0]);
            let back = bounds.upper.unwrap().value;
            let tolerance = value / 100;
            assert!(
                back.abs_diff(value) <= tolerance,
                "{lit}: {back} vs {value}"
            );
        }
    }

    #[test]
    fn unit_format() {
        assert_eq!(Unit::Duration.format(999), "999us");
        assert_eq!(Unit::Duration.format(12_250), "12.2ms");
        assert_eq!(Unit::Duration.format(1_500_000), "1.50s");
        assert_eq!(Unit::Bytes.format(512), "512B");
        assert_eq!(Unit::Bytes.format(3 * 1024 * 1024), "3.0MiB");
        assert_eq!(Unit::Count.format(7), "7");
    }
}

//! Rewriting test files from what a run measured: `--update-decorators`
//! loosens the `#attest(...)` bounds a run broke, `--add-decorators` writes a
//! block above each test that has none.
//!
//! Both are planned from the [`TestResult`]s a run returns and applied once
//! the run is over, so an interrupted run never writes anything. An edit is
//! as small as it can be: an update replaces the one number that was wrong
//! (`10ms` in `cpu < 10ms, pids <= 3`), an addition inserts one line. Lines
//! are never removed and never changed in length by anything but an update's
//! own number, so every edit's position stays what the parse said it was —
//! as long as the file is still the one that was parsed, which `apply` checks
//! before touching it.
//!
//! A new bound is the measurement with headroom: ×1.25 rounded up to two
//! significant figures for an upper bound, ×0.8 rounded down for a lower one,
//! so ordinary jitter does not break the bound on the next run. A count
//! (`pids`) gets no headroom: how many processes a test starts is not noisy.
//! With `--repeat`, the samples of one condition are folded first — an upper
//! bound from the largest, a lower from the smallest.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::decorator::{Bound, Detail, Metric, Unit};
use crate::output::{errln, outln};
use crate::parser::TestFile;
use crate::runner::TestResult;

/// One change to one file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Edit {
    pub path: PathBuf,
    pub kind: EditKind,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EditKind {
    /// Replace `old` at `span` of `line` (1-based) with `new`.
    Replace {
        line: usize,
        span: std::ops::Range<usize>,
        old: String,
        new: String,
        /// The condition as it was and as it will be, for the summary.
        condition: (String, String),
    },
    /// Insert `text` as a new line above `line` (1-based).
    Insert { line: usize, text: String },
}

/// The bound that admits every one of `samples` with headroom, in the base
/// unit of `unit`.
///
/// Headroom is multiplicative and then rounded *away* from the samples to
/// two significant figures, so the number reads as a chosen one (`16ms`, not
/// `15.25ms`) and never lands back on a sample. A count is exact: the bound
/// is the extreme sample itself (inclusive) or one past it. `None` only for
/// the one bound no value satisfies — a strict lower bound under a sample of
/// zero, since nothing is less than nothing.
pub fn bound_for(unit: Unit, upper: bool, inclusive: bool, samples: &[u64]) -> Option<u64> {
    let extreme = if upper {
        samples.iter().copied().max()
    } else {
        samples.iter().copied().min()
    }
    .expect("at least one sample");

    let candidate = if unit == Unit::Count {
        exact_past(extreme, upper, inclusive)
    } else if upper {
        round_sig(extreme as f64 * 1.25, true)
    } else {
        round_sig(extreme as f64 * 0.8, false)
    };

    // Rounding cannot move a bound back onto the samples, and headroom on
    // zero is still zero: whatever came out, every sample has to pass it.
    let admits = |value: u64| {
        let b = Bound {
            value,
            inclusive,
            literal: Default::default(),
        };
        samples.iter().all(|&s| b.admits(s, upper))
    };
    if admits(candidate) {
        return Some(candidate);
    }
    let exact = exact_past(extreme, upper, inclusive);
    admits(exact).then_some(exact)
}

/// The tightest bound on `extreme`: itself when inclusive, one base unit
/// beyond it when not.
fn exact_past(extreme: u64, upper: bool, inclusive: bool) -> u64 {
    match (inclusive, upper) {
        (true, _) => extreme,
        (false, true) => extreme + 1,
        (false, false) => extreme.saturating_sub(1),
    }
}

/// `value` rounded to two significant figures, up or down.
fn round_sig(value: f64, up: bool) -> u64 {
    if value < 10.0 {
        return if up { value.ceil() } else { value.floor() } as u64;
    }
    let magnitude = 10f64.powi(value.log10().floor() as i32 - 1);
    let scaled = value / magnitude;
    let rounded = if up { scaled.ceil() } else { scaled.floor() };
    (rounded * magnitude).round() as u64
}

/// Whether `--update-decorators` can rewrite the condition `v` broke so that
/// it admits what was measured — the test that broke it is only excused from
/// failing if so.
///
/// A set condition has no number to loosen, and a quantity has none when no
/// value satisfies its bound for this measurement. The latter is decided on
/// this one sample, which agrees with what [`plan_updates`] decides over all
/// of a `--repeat`'s: folding only moves the extreme further from the bound,
/// so a sample nothing admits leaves the fold with nothing either.
pub fn updatable(v: &crate::decorator::Violation) -> bool {
    match &v.detail {
        Detail::Quantity {
            metric,
            measured,
            broken,
        } => bound_for(
            metric.unit(),
            broken.upper,
            broken.bound.inclusive,
            &[*measured],
        )
        .is_some(),
        Detail::Set { .. } => false,
    }
}

/// A literal to rewrite, keyed by where it is: the same condition of the
/// same file, however many times `--repeat` ran its test.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
struct LiteralKey {
    path: PathBuf,
    line: usize,
    start: usize,
}

/// The edits `--update-decorators` makes: one per bound a run broke, loosened
/// to admit every measurement of it.
pub fn plan_updates(results: &[TestResult]) -> Vec<Edit> {
    struct Broken<'a> {
        violation: &'a crate::decorator::Violation,
        metric: crate::decorator::Metric,
        bound: &'a crate::decorator::Bound,
        upper: bool,
        samples: Vec<u64>,
    }
    let mut by_literal: BTreeMap<LiteralKey, Broken> = BTreeMap::new();
    let mut unwritable: BTreeSet<(PathBuf, usize, String)> = BTreeSet::new();
    for result in results {
        for v in &result.violations {
            // A set condition has no number to loosen: which syscalls or
            // paths a test may use is a decision, not a measurement, so the
            // violation is reported and the condition left as written.
            let Detail::Quantity {
                metric,
                measured,
                broken,
            } = &v.detail
            else {
                unwritable.insert((
                    result.file.canonical_path.clone(),
                    v.line,
                    v.condition.clone(),
                ));
                continue;
            };
            let key = LiteralKey {
                path: result.file.canonical_path.clone(),
                line: broken.bound.literal.line,
                start: broken.bound.literal.span.start,
            };
            by_literal
                .entry(key)
                .or_insert_with(|| Broken {
                    violation: v,
                    metric: *metric,
                    bound: &broken.bound,
                    upper: broken.upper,
                    samples: Vec::new(),
                })
                .samples
                .push(*measured);
        }
    }
    for (path, line, condition) in unwritable {
        errln!(
            "warning: {}:{}: `{}` did not hold, and has no number to update",
            path.display(),
            line,
            condition.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }

    by_literal
        .into_iter()
        .filter_map(|(key, broken)| {
            let v = broken.violation;
            let bound = broken.bound;
            let unit = broken.metric.unit();
            let Some(new_value) = bound_for(unit, broken.upper, bound.inclusive, &broken.samples)
            else {
                errln!(
                    "warning: {}:{}: no value satisfies `{}` for what was measured; not updated",
                    key.path.display(),
                    key.line,
                    v.condition
                );
                return None;
            };
            let new = unit.literal(new_value);
            let old = bound.literal.text.clone();
            // A condition written across lines is shown on one in the
            // summary.
            let before = v.condition.split_whitespace().collect::<Vec<_>>().join(" ");
            let after = before.replacen(&old, &new, 1);
            let condition = (before, after);
            Some(Edit {
                path: key.path,
                kind: EditKind::Replace {
                    line: key.line,
                    span: bound.literal.span.clone(),
                    old,
                    new,
                    condition,
                },
            })
        })
        .collect()
}

/// The edits `--add-decorators` makes: a block above every test that has
/// none, with a condition for each of [`Metric::GENERATED`] the run measured.
///
/// A test is skipped if any run of it failed (its numbers are
/// not those of a complete run), if the parser gave it no line, if it has a
/// block already, or if nothing was measured.
pub fn plan_additions(results: &[TestResult]) -> Vec<Edit> {
    let mut unmeasured = 0usize;
    struct Measured<'a> {
        file: &'a TestFile,
        samples: [Vec<u64>; Metric::GENERATED.len()],
        complete: bool,
    }
    let mut by_test: BTreeMap<(PathBuf, &str), Measured> = BTreeMap::new();
    for result in results {
        let key = (result.file.canonical_path.clone(), result.test);
        let entry = by_test.entry(key).or_insert_with(|| Measured {
            file: result.file,
            samples: Default::default(),
            complete: true,
        });
        if !result.commands_succeeded() {
            entry.complete = false;
            continue;
        }
        for (i, metric) in Metric::GENERATED.iter().enumerate() {
            if let Some(value) = metric.measure(result.duration, result.resources.as_ref()) {
                entry.samples[i].push(value);
            }
        }
    }

    let mut edits = Vec::new();
    for ((path, test), measured) in by_test {
        if !measured.complete {
            continue;
        }
        let file = measured.file;
        if file.has_decorator(test) {
            continue;
        }
        let Some(&line) = file.test_lines.get(test) else {
            continue;
        };
        let conditions: Vec<String> = Metric::GENERATED
            .iter()
            .zip(&measured.samples)
            .filter(|(_, samples)| !samples.is_empty())
            .filter_map(|(metric, samples)| {
                let inclusive = metric.generated_inclusive();
                // An upper bound always has a value, so this cannot be `None`.
                let value = bound_for(metric.unit(), true, inclusive, samples)?;
                let op = if inclusive { "<=" } else { "<" };
                Some(format!(
                    "{} {op} {}",
                    metric.name(),
                    metric.unit().literal(value)
                ))
            })
            .collect();
        if conditions.is_empty() {
            unmeasured += 1;
            continue;
        }
        // Indented like the definition it sits above.
        let indent: String = file
            .script
            .lines()
            .nth(line - 1)
            .map(|l| l[..l.len() - l.trim_start().len()].to_string())
            .unwrap_or_default();
        edits.push(Edit {
            path,
            kind: EditKind::Insert {
                line,
                text: format!("{indent}# attest({})", conditions.join(", ")),
            },
        });
    }
    if unmeasured > 0 {
        // The per-metric "not checked" warning says why in the update case;
        // a test with no block has no condition to be warned about.
        errln!(
            "warning: no decorator added for {unmeasured} test{}: the run measured none of {} (cgroup v2 unavailable or the controllers are not enabled; -d says which)",
            plural(unmeasured),
            Metric::GENERATED
                .iter()
                .map(|m| m.name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    edits
}

/// An edit that was written, for the summary.
pub struct Applied {
    pub path: PathBuf,
    pub line: usize,
    pub added: bool,
    pub what: String,
}

/// Write `edits` into their files. Every edit of a file is applied to one
/// read of it, bottom-up, and the file is written back once; a file whose
/// text no longer has an update's old literal where the parse put it has
/// changed since, and is left alone with a warning rather than guessed at.
pub fn apply(edits: Vec<Edit>) -> Vec<Applied> {
    let mut by_file: BTreeMap<PathBuf, Vec<EditKind>> = BTreeMap::new();
    for edit in edits {
        by_file.entry(edit.path).or_default().push(edit.kind);
    }

    let mut applied = Vec::new();
    for (path, mut kinds) in by_file {
        match apply_to_file(&path, &mut kinds) {
            Ok(done) => applied.extend(done),
            Err(e) => errln!("warning: {}: {e}; not updated", path.display()),
        }
    }
    applied
}

fn apply_to_file(path: &Path, kinds: &mut [EditKind]) -> anyhow::Result<Vec<Applied>> {
    let text = std::fs::read_to_string(path)?;
    // Keep the file's own line endings out of the arithmetic: lines are
    // split on `\n` and joined back with it, so a `\r` stays on its line.
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();

    // Bottom-up, and right-to-left within a line, so no edit moves another.
    // Inserting above a line and replacing within it are ordered insert
    // first at the same line number: the replacement's line is then one
    // further down, which is where the already-inserted text put it... but
    // only if the insert went first. Do replaces first instead, so neither
    // has to account for the other.
    kinds.sort_by_key(|k| match k {
        EditKind::Replace { line, span, .. } => (*line, 1, span.start),
        EditKind::Insert { line, .. } => (*line, 0, 0),
    });

    // Validate every replace before writing anything.
    for kind in kinds.iter() {
        if let EditKind::Replace {
            line, span, old, ..
        } = kind
        {
            let have = lines
                .get(line - 1)
                .and_then(|l| l.get(span.clone()))
                .unwrap_or("");
            if have != old {
                anyhow::bail!(
                    "line {line} no longer reads `{old}` where it was parsed (the file changed during the run?)"
                );
            }
        }
    }

    let mut applied = Vec::new();
    for kind in kinds.iter().rev() {
        match kind {
            EditKind::Replace {
                line,
                span,
                new,
                condition,
                ..
            } => {
                lines[line - 1].replace_range(span.clone(), new);
                applied.push(Applied {
                    path: path.to_path_buf(),
                    line: *line,
                    added: false,
                    what: format!("{}  ->  {}", condition.0, condition.1),
                });
            }
            EditKind::Insert { line, text } => {
                lines.insert(line - 1, text.clone());
                applied.push(Applied {
                    path: path.to_path_buf(),
                    line: *line,
                    added: true,
                    what: format!("+ {}", text.trim_start()),
                });
            }
        }
    }
    // `fs::write` truncates in place, so the file keeps its inode and mode
    // — an executable test file stays executable.
    std::fs::write(path, lines.join("\n"))?;
    applied.reverse();
    Ok(applied)
}

/// Report what was written. On stdout, where the run's own summary went, or
/// on stderr under `--json` so the JSONL stays parseable.
pub fn print_summary(applied: &[Applied], json: bool) {
    let added = applied.iter().filter(|a| a.added).count();
    let updated = applied.len() - added;
    let files = applied
        .iter()
        .map(|a| &a.path)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let headline = match (updated, added) {
        (0, 0) => "No decorators changed".to_string(),
        (u, 0) => format!(
            "Updated {u} condition{} in {files} file{}:",
            plural(u),
            plural(files)
        ),
        (0, a) => format!(
            "Added {a} decorator{} in {files} file{}:",
            plural(a),
            plural(files)
        ),
        (u, a) => format!(
            "Updated {u} condition{}, added {a} decorator{} in {files} file{}:",
            plural(u),
            plural(a),
            plural(files)
        ),
    };
    // Paths are the canonical ones the results carry; under the current
    // directory they read better the way the run itself names the file.
    let cwd = std::env::current_dir().ok();
    let mut out = String::new();
    out.push('\n');
    out.push_str(&headline);
    out.push('\n');
    for a in applied {
        let shown = cwd
            .as_deref()
            .and_then(|cwd| a.path.strip_prefix(cwd).ok())
            .unwrap_or(&a.path)
            .to_string_lossy();
        let path = crate::output::escape_label(&shown);
        out.push_str(&format!("  {path}:{}  {}\n", a.line, a.what));
    }
    if json {
        errln!("{}", out.trim_end());
    } else {
        outln!("{}", out.trim_end());
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::ResourceStats;
    use crate::decorator::{Broken, Literal, Violation};
    use crate::runner::Outcome;
    use std::time::Duration;

    #[test]
    fn upper_bounds_get_headroom_rounded_to_two_figures() {
        // 12.2ms × 1.25 = 15.25ms, up to 16ms.
        assert_eq!(
            bound_for(Unit::Duration, true, false, &[12_200]),
            Some(16_000)
        );
        // Across repeats the largest sample sets it: 9ms × 1.25 = 11.25, 12ms.
        assert_eq!(
            bound_for(Unit::Duration, true, false, &[4_000, 9_000, 7_000]),
            Some(12_000)
        );
        // Rounding is on the base unit: 4.3MiB × 1.25 = 5,636,096B, up to
        // 5,700,000.
        assert_eq!(
            bound_for(Unit::Bytes, true, false, &[4_508_877]),
            Some(5_700_000)
        );
        // Small values round at the integer.
        assert_eq!(bound_for(Unit::Duration, true, false, &[3]), Some(4));
    }

    #[test]
    fn lower_bounds_get_headroom_the_other_way() {
        // 12.2ms × 0.8 = 9.76ms, down to 9.7ms.
        assert_eq!(
            bound_for(Unit::Duration, false, false, &[12_200]),
            Some(9_700)
        );
        // The smallest sample sets it.
        assert_eq!(
            bound_for(Unit::Duration, false, true, &[12_200, 5_000, 8_000]),
            Some(4_000)
        );
    }

    #[test]
    fn counts_are_exact() {
        assert_eq!(bound_for(Unit::Count, true, true, &[3, 5, 4]), Some(5));
        assert_eq!(bound_for(Unit::Count, true, false, &[3, 5, 4]), Some(6));
        assert_eq!(bound_for(Unit::Count, false, true, &[3, 5, 4]), Some(3));
        assert_eq!(bound_for(Unit::Count, false, false, &[3, 5, 4]), Some(2));
    }

    #[test]
    fn a_bound_always_admits_every_sample() {
        // Headroom on zero is zero, which `< 0` cannot admit: fall back to
        // the exact bound past it.
        assert_eq!(bound_for(Unit::Duration, true, false, &[0]), Some(1));
        assert_eq!(bound_for(Unit::Bytes, true, true, &[0]), Some(0));
        // A lower bound that would round to the sample itself under `>`.
        assert_eq!(bound_for(Unit::Duration, false, false, &[1]), Some(0));
        // And the one bound nothing satisfies.
        assert_eq!(bound_for(Unit::Duration, false, false, &[0]), None);
        for samples in [&[0u64][..], &[1], &[9], &[10], &[99], &[100], &[123_456]] {
            for (upper, inclusive) in [(true, true), (true, false), (false, true), (false, false)] {
                let Some(value) = bound_for(Unit::Duration, upper, inclusive, samples) else {
                    assert_eq!((samples, upper, inclusive), (&[0u64][..], false, false));
                    continue;
                };
                let b = Bound {
                    value,
                    inclusive,
                    literal: Literal::default(),
                };
                for &s in samples {
                    assert!(
                        b.admits(s, upper),
                        "{samples:?} {upper} {inclusive} -> {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn round_sig_rounds_to_two_figures() {
        assert_eq!(round_sig(15_250.0, true), 16_000);
        assert_eq!(round_sig(15_250.0, false), 15_000);
        assert_eq!(round_sig(9.76, true), 10);
        assert_eq!(round_sig(9.76, false), 9);
        assert_eq!(round_sig(100.0, true), 100);
        assert_eq!(round_sig(101.0, true), 110);
        assert_eq!(round_sig(99.0, false), 99);
    }

    /// A passed result for `test` in `path` with one violation of `condition`
    /// (`cpu < 10ms` style, the literal being the last token) that measured
    /// `measured`.
    fn violated<'a>(
        file: &'a TestFile,
        test: &'a str,
        line: usize,
        condition: &str,
        measured: u64,
    ) -> TestResult<'a> {
        let lit = condition.rsplit(' ').next().unwrap();
        let col = 9; // after `# attest(`
        let lit_start = col + condition.len() - lit.len();
        let upper = condition.contains('<');
        let value = crate::decorator::scan(&format!("# attest({condition})"))
            .unwrap()
            .pop()
            .unwrap()
            .conditions
            .pop()
            .map(|c| match c.kind {
                crate::decorator::Kind::Quantity { bounds, .. } => {
                    if upper { bounds.upper } else { bounds.lower }
                        .unwrap()
                        .value
                }
                _ => unreachable!(),
            })
            .unwrap();
        let mut r = result(file, test);
        r.violations.push(Violation {
            line,
            span: col..col + condition.len(),
            condition: condition.to_string(),
            message: String::new(),
            detail: Detail::Quantity {
                metric: Metric::Cpu,
                measured,
                broken: Broken {
                    bound: Bound {
                        value,
                        inclusive: condition.contains("<=") || condition.contains(">="),
                        literal: Literal {
                            line,
                            span: lit_start..lit_start + lit.len(),
                            text: lit.to_string(),
                        },
                    },
                    upper,
                },
            },
        });
        r
    }

    fn result<'a>(file: &'a TestFile, test: &'a str) -> TestResult<'a> {
        TestResult {
            name: test.to_string(),
            test,
            file,
            outcome: Outcome::Pass,
            duration: Duration::from_millis(1),
            context: PathBuf::from("/nonexistent"),
            resources: None,
            syscalls: None,
            violations: Vec::new(),
        }
    }

    #[test]
    fn updates_fold_repeats_and_replace_only_the_literal() {
        let tf = TestFile {
            path: "a.test".into(),
            ..Default::default()
        };
        let results = vec![
            violated(&tf, "test_x", 3, "cpu < 10ms", 12_200),
            violated(&tf, "test_x", 3, "cpu < 10ms", 14_000),
            violated(&tf, "test_x", 3, "cpu < 10ms", 11_000),
        ];
        let edits = plan_updates(&results);
        assert_eq!(edits.len(), 1);
        let EditKind::Replace {
            line,
            span,
            old,
            new,
            condition,
        } = &edits[0].kind
        else {
            panic!()
        };
        assert_eq!((*line, span.clone()), (3, 15..19));
        assert_eq!(old, "10ms");
        // max 14ms × 1.25 = 17.5ms → 18ms.
        assert_eq!(new, "18ms");
        assert_eq!(condition.1, "cpu < 18ms");
    }

    #[test]
    fn only_a_violation_with_a_number_to_rewrite_is_updatable() {
        let tf = TestFile::default();
        let quantity = |condition, measured| {
            violated(&tf, "test_x", 1, condition, measured)
                .violations
                .pop()
                .unwrap()
        };
        assert!(updatable(&quantity("cpu < 10ms", 12_200)));
        assert!(updatable(&quantity("cpu > 10ms", 5)));
        // Nothing is less than nothing: `> x` under a sample of 0 has no
        // rewrite, and `plan_updates` skips it.
        assert!(!updatable(&quantity("cpu > 10ms", 0)));
        let mut set = quantity("cpu < 10ms", 12_200);
        set.detail = Detail::Set {
            offenders: vec!["/tmp/zzz".into()],
        };
        assert!(!updatable(&set));
    }

    #[test]
    fn apply_rewrites_in_place_and_keeps_the_rest_of_the_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("a.test");
        let source = "#!/bin/sh\n\n# attest(cpu < 10ms, 2 <= pids)\ntest_x() {\n\ttrue\n}\n";
        std::fs::write(&path, source).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let p = path.to_str().unwrap();
        let edits = vec![
            // Two replacements on one line: right-to-left keeps the left
            // one's span valid.
            Edit {
                path: path.clone(),
                kind: EditKind::Replace {
                    line: 3,
                    span: 15..19,
                    old: "10ms".into(),
                    new: "16ms".into(),
                    condition: ("cpu < 10ms".into(), "cpu < 16ms".into()),
                },
            },
            Edit {
                path: path.clone(),
                kind: EditKind::Replace {
                    line: 3,
                    span: 21..22,
                    old: "2".into(),
                    new: "1".into(),
                    condition: ("2 <= pids".into(), "1 <= pids".into()),
                },
            },
        ];
        let applied = apply(edits);
        assert_eq!(applied.len(), 2, "{p}");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after,
            "#!/bin/sh\n\n# attest(cpu < 16ms, 1 <= pids)\ntest_x() {\n\ttrue\n}\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
    }

    #[test]
    fn apply_leaves_a_file_that_changed_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("a.test");
        let source = "# attest(cpu < 99ms)\ntest_x() {\n\ttrue\n}\n";
        std::fs::write(&path, source).unwrap();
        let edits = vec![Edit {
            path: path.clone(),
            kind: EditKind::Replace {
                line: 1,
                span: 15..19,
                old: "10ms".into(),
                new: "16ms".into(),
                condition: ("cpu < 10ms".into(), "cpu < 16ms".into()),
            },
        }];
        assert!(apply(edits).is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }

    #[test]
    fn inserts_and_replaces_in_one_file_do_not_move_each_other() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("a.test");
        let source = "test_a() {\n\ttrue\n}\n\n# attest(cpu < 10ms)\ntest_b() {\n\ttrue\n}\n\n  test_c() {\n\ttrue\n}\n";
        std::fs::write(&path, source).unwrap();
        let edits = vec![
            Edit {
                path: path.clone(),
                kind: EditKind::Insert {
                    line: 10,
                    text: "  # attest(pids <= 2)".into(),
                },
            },
            Edit {
                path: path.clone(),
                kind: EditKind::Replace {
                    line: 5,
                    span: 15..19,
                    old: "10ms".into(),
                    new: "16ms".into(),
                    condition: ("cpu < 10ms".into(), "cpu < 16ms".into()),
                },
            },
            Edit {
                path: path.clone(),
                kind: EditKind::Insert {
                    line: 1,
                    text: "# attest(cpu < 2ms)".into(),
                },
            },
        ];
        let applied = apply(edits);
        assert_eq!(applied.len(), 3);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# attest(cpu < 2ms)\ntest_a() {\n\ttrue\n}\n\n# attest(cpu < 16ms)\ntest_b() {\n\ttrue\n}\n\n  # attest(pids <= 2)\n  test_c() {\n\ttrue\n}\n"
        );
        // Reported top-down, at the line numbers of the file as it was.
        let lines: Vec<usize> = applied.iter().map(|a| a.line).collect();
        assert_eq!(lines, [1, 5, 10]);
    }

    fn parsed(dir: &Path, name: &str, source: &str) -> TestFile {
        let path = dir.join(name);
        std::fs::write(&path, source).unwrap();
        crate::parser::parse_test_file(&path, &crate::parser::Prescan::default()).unwrap()
    }

    #[test]
    fn additions_cover_tests_without_a_block_from_what_was_measured() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tf = parsed(
            tmp.path(),
            "a.test",
            "#!/bin/sh\n\n# attest(cpu < 10ms)\ntest_has() {\n\ttrue\n}\n\n## documented\ntest_none() {\n\ttrue\n}\n\ntest_failed() {\n\tfalse\n}\n\ntest_unmeasured() {\n\ttrue\n}\n",
        );
        let stats = ResourceStats {
            cpu_user_usec: Some(8_000),
            cpu_system_usec: Some(4_000),
            memory_peak: Some(3 * 1024 * 1024),
            pids_peak: Some(4),
            ..Default::default()
        };
        let with = |test: &'static str, pids: u64| {
            let mut r = result(&tf, test);
            r.resources = Some(ResourceStats {
                pids_peak: Some(pids),
                ..stats.clone()
            });
            r
        };
        let mut failed = with("test_failed", 1);
        failed.outcome = Outcome::Fail;
        let results = vec![
            with("test_has", 1),
            with("test_none", 4),
            with("test_none", 6),
            failed,
            result(&tf, "test_unmeasured"),
        ];
        let edits = plan_additions(&results);
        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(
            edits[0].kind,
            EditKind::Insert {
                line: 9,
                // cpu 12ms × 1.25 = 15ms; mem 3MiB × 1.25 = 3.75MiB → 4,000,000B
                // = 3.81MiB; pids max 6.
                text: "# attest(cpu < 15ms, mem < 3.81MiB, pids <= 6)".into(),
            }
        );
    }

    /// What is written has to be what the parser reads back: a block above
    /// the right test, through a doc comment, at the test's own indentation.
    #[test]
    fn an_added_block_parses_back_onto_its_test() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tf = parsed(
            tmp.path(),
            "a.test",
            "#!/bin/sh\n\n## documented\ntest_a() {\n\ttrue\n}\n\nif true; then\n  test_b() {\n    true\n  }\nfi\n",
        );
        let stats = ResourceStats {
            cpu_user_usec: Some(1_000),
            cpu_system_usec: Some(1_000),
            pids_peak: Some(2),
            ..Default::default()
        };
        let results: Vec<TestResult> = ["test_a", "test_b"]
            .into_iter()
            .map(|t| {
                let mut r = result(&tf, t);
                r.resources = Some(stats.clone());
                r
            })
            .collect();
        let edits = plan_additions(&results);
        assert_eq!(edits.len(), 2);
        let applied = apply(edits);
        assert_eq!(applied.len(), 2);

        // The block goes directly above the definition, under a doc comment
        // rather than over it: the line above a definition is known to be
        // the definition's, while a comment further up may belong to
        // whatever came before.
        let after = std::fs::read_to_string(&tf.path).unwrap();
        assert_eq!(
            after,
            "#!/bin/sh\n\n## documented\n# attest(cpu < 2.5ms, pids <= 2)\ntest_a() {\n\ttrue\n}\n\nif true; then\n  # attest(cpu < 2.5ms, pids <= 2)\n  test_b() {\n    true\n  }\nfi\n"
        );
        let reparsed =
            crate::parser::parse_test_file(&tf.path, &crate::parser::Prescan::default()).unwrap();
        for test in ["test_a", "test_b"] {
            let texts: Vec<&str> = reparsed
                .conditions_of(test)
                .map(|c| c.text.as_str())
                .collect();
            assert_eq!(texts, ["cpu < 2.5ms", "pids <= 2"], "{test}");
        }
    }

    #[test]
    fn additions_skip_metrics_the_run_did_not_measure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tf = parsed(tmp.path(), "a.test", "test_x() {\n\ttrue\n}\n");
        let mut r = result(&tf, "test_x");
        r.resources = Some(ResourceStats {
            pids_peak: Some(2),
            ..Default::default()
        });
        let edits = plan_additions(&[r]);
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].kind,
            EditKind::Insert {
                line: 1,
                text: "# attest(pids <= 2)".into(),
            }
        );
    }
}

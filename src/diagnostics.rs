use std::path::Path;

use crate::output::{Palette, out_palette, outln};
use crate::parser::TestFile;
use crate::runner::{Outcome, TestResult};

/// Information extracted from an xtrace log about the failing command.
pub(crate) struct FailureInfo {
    /// 1-based line number where the failure occurred. The runner script
    /// preserves the source's line numbers, so this is also the line in the
    /// source file.
    pub(crate) lineno: usize,
    /// The command text as shown in xtrace (e.g. `'[' ABC = DEF ']'`), or
    /// empty when the shell gave up on `lineno` before tracing it (see
    /// [`parse_shell_error_line`]).
    command: String,
    /// 0-based inclusive line range of the function `lineno` sits in.
    function: (usize, usize),
}

/// A parsed `[` (test) expression from xtrace output.
struct BracketExpr {
    left: String,
    op: String,
    right: String,
}

/// Print a source snippet showing where a failed test went wrong. `file` is the
/// parse the test was run from.
///
/// The lines rendered are the script's ([`TestFile::script`]): the xtrace line
/// number is the shell's `$LINENO`, which counts lines of the script it is
/// running, and that script is the file's function definitions at their own
/// line numbers with nothing in front of them — so the number indexes the
/// source directly, and the script *is* the source wherever a function is.
/// Taking the lines, and the function bounds that clamp the window around
/// them, from the parse rather than reading the file back means they come from
/// the one text the test actually ran: no second read, no second shell parse
/// per failure, and no way for the file to have changed in between.
pub fn print_failure_snippet(result: &TestResult) {
    let file = result.file;
    let Some(failure) = parse_xtrace_failure(&result.context, file) else {
        return;
    };

    let lines: Vec<&str> = file.script.lines().collect();
    let line_idx = failure.lineno - 1;
    if line_idx >= lines.len() {
        return;
    }

    // A timed-out test did not fail a command; the highlighted line is simply
    // where it was still running when the clock ran out.
    let title = if result.outcome == Outcome::Timeout {
        "test timed out"
    } else {
        "command failed"
    };

    // Clamp the rendered context to the enclosing function so a snippet never
    // leaks into an adjacent test.
    let (func_start_line, func_end_line) = failure.function;
    let start = line_idx.saturating_sub(CONTEXT_LINES).max(func_start_line);
    let end = (line_idx + CONTEXT_LINES + 1)
        .min(lines.len())
        .min(func_end_line + 1);

    outln!(
        "{}",
        render_snippet(
            title,
            &result.file.path,
            &lines,
            Highlight::line(line_idx),
            start..end,
            out_palette().is_colored(),
        )
    );

    // If the failing command is a `[` expression, show operand details
    if let Some(expr) = parse_bracket_expr(&failure.command) {
        render_bracket_diff(&expr);
    }
}

/// Report each `#attest(...)` condition the test failed, with a snippet that
/// points at the condition itself.
///
/// The test's commands all succeeded — that is the only case in which
/// conditions are checked — so there is no command to blame and the trace
/// holds no failure to replay. What there is to show is the assertion, which
/// the script carries at its own line number (the parser keeps decorator
/// lines, see `parser::build_functions_source`), and the number that broke it,
/// which is the snippet's label. The window runs from the condition down past
/// the definition of the test it decorates ([`violation_window`]), so the
/// test's name is in view too.
pub fn print_violations(result: &TestResult) {
    crate::output::write_out(violations_report(result, out_palette().is_colored()).as_bytes());
}

/// The text [`print_violations`] writes, built whole so it can be checked.
fn violations_report(result: &TestResult, colored: bool) -> String {
    use std::fmt::Write as _;

    let file = result.file;
    let lines: Vec<&str> = file.script.lines().collect();
    let mut out = String::new();
    for violation in &result.violations {
        // A `writes` message and its offenders are paths the test itself
        // created, so they are whatever bytes it chose: `touch $'\e[2K…'`
        // must not get to rewrite the report it is named in. The label is
        // not measured against the source, so escaping it moves no span.
        let message = crate::output::escape_label(&violation.message);
        let line_idx = violation.line - 1;
        if line_idx >= lines.len() {
            let _ = writeln!(
                out,
                "error: {} (asserted `{}` at line {})",
                message, violation.condition, violation.line
            );
            continue;
        }
        let snippet = render_snippet(
            "assertion on the run failed",
            &result.file.path,
            &lines,
            Highlight {
                line: line_idx,
                span: Some(violation.span.clone()),
                label: Some(message.as_ref()),
            },
            violation_window(&file.functions, line_idx, lines.len()),
            colored,
        );
        let _ = writeln!(out, "{snippet}");
        // A set violation's label spells its first few offenders; the rest
        // follow the snippet, one per line, so a test that wrote to twenty
        // places is told all twenty.
        if let crate::decorator::Detail::Set { offenders } = &violation.detail
            && offenders.len() > crate::decorator::SHOWN_OFFENDERS
        {
            for offender in offenders {
                let _ = writeln!(out, "  {}", crate::output::escape_label(offender));
            }
            out.push('\n');
        }
    }
    out
}

/// The lines a violation's snippet shows: from the condition on `line` down to
/// the test it decorates, given the script's function bounds and length.
///
/// The decorator sits above its function, outside every bound; the function
/// it belongs to is the first one that starts below it. A block spread over
/// several lines, or a doc comment under it, can put that definition further
/// below the condition than the [`CONTEXT_LINES`] a failed command gets, and
/// the window reaches it all the same — the definition and the line after it —
/// since the test's name is what says what the condition is about. It never
/// runs past the end of that function.
fn violation_window(
    functions: &[(usize, usize)],
    line: usize,
    len: usize,
) -> std::ops::Range<usize> {
    let (func_start, func_end) = functions
        .iter()
        .find(|&&(start, _)| start > line)
        .map_or((line, len - 1), |&bounds| bounds);
    let end = (line + CONTEXT_LINES + 1)
        .max(func_start + 2)
        .min(func_end + 1)
        .min(len);
    line..end
}

/// What a snippet points at: a line of the script, optionally only a byte
/// range of it, optionally with a label.
struct Highlight<'a> {
    /// 0-based line index.
    line: usize,
    /// Byte range within the line. `None` highlights the whole line past its
    /// indentation.
    span: Option<std::ops::Range<usize>>,
    label: Option<&'a str>,
}

impl Highlight<'_> {
    fn line(line: usize) -> Self {
        Self {
            line,
            span: None,
            label: None,
        }
    }
}

/// Parse the xtrace log to find the command the test failed at: the last one
/// the test's own body ran.
///
/// Every command a test runs is a line of one of `file`'s functions — the
/// script the shell is given ([`TestFile::script`]) is nothing but those
/// definitions, rendered at their own line numbers, and one call. A top-level
/// trace line whose number falls inside no function is therefore not the
/// test's: it is either that call, which is always the first line of the log,
/// or an `EXIT` trap action. The shell runs a trap string after the function
/// it was set in has already been unwound, with `$LINENO` counting from the
/// string's own first line, so `trap 'rm -rf "$tmp"' EXIT` traces as
/// `+1: rm -rf /tmp/…` after the command that actually failed — and used to
/// have the snippet point at line 1 of the file, the shebang. The body is over
/// once such a line appears, so the scan ends there: what follows is the trap
/// running, whether it names a function (whose lines *are* in the script) or
/// not, and the failure belongs to the last command of the body before it.
///
/// `DEBUG` and `ERR` actions are not this case — bash traces them one `+`
/// deeper, like a subshell, and [`parse_trace_line`] already skips those.
///
/// A file with no shebang may open a function on line 1, and then the trap's
/// `+1:` *is* inside one — and used to put the snippet into whatever function
/// that was, often another test entirely. That line is the function's header,
/// though, which the body never traces ([`TestFile::headers`]), so it ends the
/// scan the same way. What is still ambiguous is a one-line function on line 1
/// (`test_x() { false; }`), whose header is also a line of its body.
///
/// The trace is not the whole story, though: the shell prints a command's
/// trace line only once it has expanded the command, so one it gives up on
/// *during* expansion — `$unset` under `set -u`, `${x:?}`, `$((1/0))`, a bad
/// substitution — never appears in it, and the last trace line is the command
/// before it, which succeeded. What the shell does print is an error naming
/// the line it stopped on, and [`parse_shell_error_line`] reads that: it
/// carries the same `$LINENO` the trace does, so it maps onto the script the
/// same way, and the last of the two kinds of line to appear is the failure.
pub(crate) fn parse_xtrace_failure(tmp_dir: &Path, file: &TestFile) -> Option<FailureInfo> {
    // Open the log the runner created for the test, never a symlink the test
    // left at that name (see `overlay::open_nofollow`).
    let log = crate::overlay::open_nofollow(&tmp_dir.join("xtrace.log")).ok()?;
    let mut reader = std::io::BufReader::new(log);

    // Walk it a line at a time rather than reading it whole: the log holds
    // everything the test and the programs it ran sent to stderr, so it is as
    // big as the test chose to make it, and the failing command's own output
    // sits between its trace line and the end of the file — a tail would miss
    // exactly the command a noisy failure needs attributing to.
    let mut last_match: Option<FailureInfo> = None;
    let mut first = true;
    let mut line = Vec::new();
    while read_capped_line(&mut reader, &mut line).unwrap_or(false) {
        // Most of a noisy log is the test's own output, which can never be a
        // trace line: settle that on the first byte, before decoding it. The
        // one other line that places the failure is the shell's own error,
        // and that one starts with the script's path.
        if line.first() != Some(&b'+') {
            if let Some(lineno) = parse_shell_error_line(&line, &file.canonical_path)
                && let Some(function) = lineno
                    .checked_sub(1)
                    .and_then(|idx| file.running_function(idx))
            {
                last_match = Some(FailureInfo {
                    lineno,
                    command: String::new(),
                    function,
                });
            }
            continue;
        }
        // Lossy because a test is free to write non-UTF-8 to its stderr, and a
        // diagnostic is better than none.
        let text = String::from_utf8_lossy(&line);
        let Some((lineno, command)) = parse_trace_line(&text) else {
            continue;
        };
        let is_call = std::mem::replace(&mut first, false);
        let function = lineno
            .checked_sub(1)
            .and_then(|idx| file.running_function(idx));
        match function {
            Some(function) => {
                last_match = Some(FailureInfo {
                    lineno,
                    command: command.to_string(),
                    function,
                })
            }
            None if is_call => {}
            None => break,
        }
    }
    last_match
}

/// The most a single line of the log is held in memory for. A trace line is a
/// command as the shell echoed it, so this is far more than one ever needs;
/// what it bounds is a *test* writing megabytes to stderr without a newline in
/// them, which is one "line" as far as this scan is concerned.
const MAX_LINE: usize = 64 * 1024;

/// Read up to the next `\n` from `reader` into `line`, keeping at most
/// [`MAX_LINE`] bytes and discarding the rest of an over-long line. Returns
/// whether anything was read at all (false at EOF).
///
/// `read_until` alone grows the buffer to hold the whole line however long
/// that is, so it reads through a `take` of the cap, and whatever of an
/// over-long line is left past it is skipped. The newline is consumed with the
/// line it ends but kept out of it, the way `str::lines` splits.
fn read_capped_line(
    reader: &mut impl std::io::BufRead,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    use std::io::{BufRead, Read};
    line.clear();
    let n = reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', line)?;
    if line.last() == Some(&b'\n') {
        line.pop();
    } else if n == MAX_LINE {
        reader.skip_until(b'\n')?;
    }
    Ok(n > 0)
}

/// One line of an xtrace log as its `$LINENO` and command, if it traces a
/// command at the top level: `+<lineno>: <command>` in the runner's `PS4`
/// format. Subshell traces (`++`, `+++`, …) and anything the test printed
/// itself are not commands a failure can be attributed to.
fn parse_trace_line(line: &str) -> Option<(usize, &str)> {
    // A `\r` before the newline belongs to the terminal conventions of
    // whatever wrote the line, not to the trace (`str::lines` drops it too).
    let rest = line.strip_suffix('\r').unwrap_or(line).strip_prefix('+')?;
    if rest.starts_with('+') {
        return None;
    }
    let (lineno, command) = rest.split_once(": ")?;
    Some((lineno.trim().parse().ok()?, command))
}

/// The line of the script a shell error message names, if `line` is one.
///
/// A shell that gives up on a command before running it — an unbound variable
/// under `set -u`, a `${x:?}`, an arithmetic error, a bad substitution — says
/// so on stderr with the line it stopped on: bash as `<$0>: line <N>: <msg>`,
/// dash as `<$0>: <N>: <msg>`. The runner passes the file's canonical path as
/// `$0` (see `runner`), so that path is what the message starts with, and
/// requiring it is what keeps a program the test ran from placing the failure:
/// `bash other.sh` reports `other.sh: line 5: …`, which names no line of this
/// script. Only the number is wanted; the message itself is already in the
/// replayed log, right under the trace.
fn parse_shell_error_line(line: &[u8], script: &Path) -> Option<usize> {
    use std::os::unix::ffi::OsStrExt;

    let rest = line
        .strip_prefix(script.as_os_str().as_bytes())?
        .strip_prefix(b": ")?;
    let rest = rest.strip_prefix(b"line ").unwrap_or(rest);
    let end = rest.iter().position(|&b| b == b':')?;
    let (digits, after) = rest.split_at(end);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) || !after.starts_with(b": ") {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// How many lines of context to show on each side of the annotated line.
const CONTEXT_LINES: usize = 3;

/// Spell the C1 controls in a line of source as `\xNN`, leaving the rest of it
/// (whitespace and layout included) exactly as written.
///
/// `annotate_snippets` substitutes the C0 controls in the source it renders — a
/// raw `ESC` in a script comes back as the `␛` picture — but passes
/// `U+0080`–`U+009F` through as ordinary text. They are not ordinary text: a
/// terminal in UTF-8 mode decodes those code points and acts on them, and
/// `U+009B` is `CSI`, an exact stand-in for `ESC [` (see
/// [`crate::output::escape_label`]). A shell script may hold them anywhere — in
/// a quoted string, in a comment — and a snippet prints the lines around the
/// failure, so without this a failing file gets to redraw the report blaming it.
fn escape_c1(line: &str) -> std::borrow::Cow<'_, str> {
    fn is_c1(c: char) -> bool {
        matches!(c, '\u{80}'..='\u{9f}')
    }

    crate::output::escape_chars(line, is_c1)
}

/// Render an annotate-snippets diagnostic highlighting `lines[highlight.line]`,
/// showing the lines in `window` (0-based, end exclusive, holding the
/// highlighted line). The caller picks the window: a failed command gets
/// [`CONTEXT_LINES`] on each side clamped to its function, so a snippet never
/// leaks into an adjacent test, while a violated condition runs down to the
/// function it decorates.
fn render_snippet(
    title: &str,
    source_path: &Path,
    lines: &[&str],
    highlight: Highlight,
    window: std::ops::Range<usize>,
    colored: bool,
) -> String {
    use annotate_snippets::{AnnotationKind, Level, Renderer, Snippet};

    let line_idx = highlight.line;
    let std::ops::Range { start, end } = window;

    // The path as discovered, spelled the way `list` spells it: the `-->`
    // line is where an editor or a terminal jumps from, so it has to name
    // the file from the directory the run was started in. The base name alone
    // named `t.test` for both `x/t.test` and `y/t.test`, and resolved against
    // nothing for a file outside the cwd. A path comes out of the directory
    // walk, so it is as much the tree's choice as a test name is:
    // annotate-snippets substitutes the control characters in the rendered
    // *source*, but prints the path it is given as-is.
    let path = crate::parser::selector_path(source_path);
    let path_str = crate::output::escape_label(&path);

    // Escaped before anything is measured, so every offset below is an offset
    // into the text that actually gets rendered (see `escape_c1`).
    let window: Vec<std::borrow::Cow<str>> =
        lines[start..end].iter().copied().map(escape_c1).collect();
    let source = window.join("\n");

    // Annotate the failing line within the rendered window — the whole of it
    // past its indentation, or the range asked for. `join("\n")` puts one byte
    // between lines.
    let line_start: usize = window[..line_idx - start].iter().map(|l| l.len() + 1).sum();
    let failing = &*window[line_idx - start];
    let indent = failing.len() - failing.trim_start().len();
    let (from, to) = match highlight.span {
        // Escaping may have moved bytes about; clamp the range into the line
        // rather than reach past it.
        Some(range) => (range.start.min(failing.len()), range.end.min(failing.len())),
        None => (indent, failing.len()),
    };
    let mut annotation = AnnotationKind::Primary.span(line_start + from..line_start + to);
    if let Some(label) = highlight.label {
        annotation = annotation.label(label);
    }

    let report = &[Level::ERROR.primary_title(title).element(
        Snippet::source(&source)
            .path(&*path_str)
            .line_start(start + 1)
            .fold(false)
            .annotation(annotation),
    )];

    let renderer = if colored {
        Renderer::styled()
    } else {
        Renderer::plain()
    };
    renderer.render(report).to_string()
}

/// Comparison operators we know how to render a diff for. `==` is how `[[`
/// usually spells `=`, though bash echoes back whichever one was written.
fn is_comparison_op(op: &str) -> bool {
    matches!(
        op,
        "=" | "==" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
    )
}

/// Parse a `[`, `test` or `[[` comparison from xtrace output.
///
/// Xtrace renders `[ "A" = "B" ]` as `'[' A = B ']'`, `test "A" = "B"` as
/// `test A = B` and `[[ "A" = "B" ]]` as `[[ A = B ]]`. `[` and `test` are the
/// same builtin spelled two ways — the only difference in the trace is the
/// `'['`/`']'` arguments bracketing the operands — so both are read the same
/// way: each operand is a separately quoted word. `[[` needs different
/// handling, being shell syntax rather than a command: its operands are echoed
/// back verbatim with no quoting at all.
fn parse_bracket_expr(command: &str) -> Option<BracketExpr> {
    let bracketed = command
        .strip_prefix("'[' ")
        .and_then(|s| s.strip_suffix(" ']'"));
    // A `test` with no argument at all cannot be a comparison, so requiring the
    // space costs nothing and keeps the operand list non-empty.
    if let Some(inner) = bracketed.or_else(|| command.strip_prefix("test ")) {
        let words = split_quoted_words(inner)?;
        let [left, op, right] = <[String; 3]>::try_from(words).ok()?;
        if !is_comparison_op(&op) {
            return None;
        }
        return Some(BracketExpr { left, op, right });
    }

    // Unquoted `[[` operands: the operator token is the only thing that marks
    // the boundary, so an operand that itself looks like an operator makes the
    // split ambiguous and we decline rather than guess.
    let inner = command
        .strip_prefix("[[ ")
        .and_then(|s| s.strip_suffix(" ]]"))?;
    let words: Vec<&str> = inner.split(' ').collect();
    // Each side needs at least one word, so the operator can't be first or last.
    let interior = 1..words.len().saturating_sub(1);
    let mut op_idx = None;
    for (i, word) in words.iter().enumerate() {
        if !interior.contains(&i) || !is_comparison_op(word) {
            continue;
        }
        if op_idx.is_some() {
            return None;
        }
        op_idx = Some(i);
    }
    let i = op_idx?;
    Some(BracketExpr {
        left: words[..i].join(" "),
        op: words[i].to_string(),
        right: words[i + 1..].join(" "),
    })
}

/// Split an xtrace-rendered argument list back into its original words.
///
/// Bash quotes any argument that needs it, so an operand holding a space is
/// printed as one `'...'` word (with an embedded `'` spelled `'\''`) and one
/// holding control characters as `$'...'`. Splitting on spaces alone would tear
/// those apart, so unquote as we go. Returns `None` if the quoting doesn't
/// parse, in which case the caller renders no diff instead of a wrong one.
fn split_quoted_words(inner: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' => words.push(std::mem::take(&mut word)),
            // Literal run: everything up to the closing quote, escapes and all.
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    ch => word.push(ch),
                }
            },
            // `$'...'`: same, but backslash escapes are interpreted.
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                loop {
                    match chars.next()? {
                        '\'' => break,
                        '\\' => word.push(unescape_ansi_c(&mut chars)?),
                        ch => word.push(ch),
                    }
                }
            }
            // Outside quotes a backslash escapes the next character.
            '\\' => word.push(chars.next()?),
            _ => word.push(c),
        }
    }
    words.push(word);
    Some(words)
}

/// Resolve one backslash escape inside a `$'...'` word, with `chars` positioned
/// just past the backslash. Unrecognized escapes stand for themselves, matching
/// how the shell reads them back.
fn unescape_ansi_c(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<char> {
    let c = chars.next()?;
    Some(match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'a' => '\x07',
        'b' => '\x08',
        'f' => '\x0c',
        'v' => '\x0b',
        'e' | 'E' => '\x1b',
        '0' => '\0',
        'x' => {
            // Up to two hex digits.
            let mut v = 0u32;
            let mut digits = 0;
            while digits < 2
                && let Some(d) = chars.peek().and_then(|c| c.to_digit(16))
            {
                v = v * 16 + d;
                digits += 1;
                chars.next();
            }
            if digits == 0 {
                return Some('x');
            }
            char::from_u32(v)?
        }
        other => other,
    })
}

/// Make an operand safe to print on a single line, so a value containing
/// newlines or tabs doesn't scramble the `left`/`right`/`diff` alignment.
fn escape_for_display(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Render a comparison between bracket expression operands.
fn render_bracket_diff(expr: &BracketExpr) {
    outln!();
    for line in bracket_diff_lines(expr, out_palette()) {
        outln!("{line}");
    }
}

/// The `left`/`right` lines for a failed comparison and, for the equality
/// operators, the character-level `diff` under them.
///
/// The diff marks the characters that differ in colour. Without colour — any
/// report that is not going to a terminal, which is to say every CI log — the
/// two `diff` lines would be the `left` and `right` lines over again, with the
/// one thing they exist to show gone. So off a terminal each of them is
/// followed by a line of `^` under the characters that differ (only when there
/// are any on that side: a right operand that is a prefix of the left one has
/// nothing of its own to point at), the way the snippet above underlines the
/// command.
fn bracket_diff_lines(expr: &BracketExpr, palette: Palette) -> Vec<String> {
    let left = escape_for_display(&expr.left);
    let right = escape_for_display(&expr.right);

    let mut lines = vec![
        format!("  left: \"{left}\""),
        format!(" right: \"{right}\""),
    ];

    // For equality operators, show inline diff if values differ
    if matches!(expr.op.as_str(), "=" | "==" | "!=") && left != right {
        use similar::{ChangeTag, TextDiff};
        let Palette {
            green, red, reset, ..
        } = palette;
        let diff = TextDiff::from_chars(&left, &right);
        let mut left_side = DiffSide::default();
        let mut right_side = DiffSide::default();
        for change in diff.iter_all_changes() {
            let val = change.value();
            match change.tag() {
                ChangeTag::Equal => {
                    left_side.push(val, None);
                    right_side.push(val, None);
                }
                ChangeTag::Delete => left_side.push(val, Some((red, reset))),
                ChangeTag::Insert => right_side.push(val, Some((green, reset))),
            }
        }
        let plain = !palette.is_colored();
        lines.push(format!("  diff: \"{}\"", left_side.text));
        if plain && left_side.marked {
            lines.push(format!("         {}", left_side.carets.trim_end()));
        }
        lines.push(format!("        \"{}\"", right_side.text));
        if plain && right_side.marked {
            lines.push(format!("         {}", right_side.carets.trim_end()));
        }
    }
    lines
}

/// One operand of a character-level diff as it is laid out: the text with the
/// changed characters wrapped in colour, and a line that puts a `^` under each
/// of them and a blank under everything else, column for column.
#[derive(Default)]
struct DiffSide {
    text: String,
    carets: String,
    /// Whether any character of this side differs from the other.
    marked: bool,
}

impl DiffSide {
    fn push(&mut self, val: &str, style: Option<(&str, &str)>) {
        use unicode_width::UnicodeWidthStr;
        let mark = if style.is_some() { "^" } else { " " };
        let width = UnicodeWidthStr::width(val);
        self.carets.push_str(&mark.repeat(width));
        self.marked |= style.is_some() && width > 0;
        match style {
            Some((on, off)) => {
                self.text.push_str(on);
                self.text.push_str(val);
                self.text.push_str(off);
            }
            None => self.text.push_str(val),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bracket_equality() {
        let expr = parse_bracket_expr("'[' ABC = DEF ']'").unwrap();
        assert_eq!(expr.left, "ABC");
        assert_eq!(expr.op, "=");
        assert_eq!(expr.right, "DEF");
    }

    #[test]
    fn parse_bracket_inequality() {
        let expr = parse_bracket_expr("'[' foo != bar ']'").unwrap();
        assert_eq!(expr.left, "foo");
        assert_eq!(expr.op, "!=");
        assert_eq!(expr.right, "bar");
    }

    #[test]
    fn parse_bracket_numeric() {
        let expr = parse_bracket_expr("'[' 1 -eq 2 ']'").unwrap();
        assert_eq!(expr.left, "1");
        assert_eq!(expr.op, "-eq");
        assert_eq!(expr.right, "2");
    }

    #[test]
    fn parse_double_bracket_equality() {
        // `[[ "A" = "B" ]]` is rendered by bash xtrace as `[[ A == B ]]`.
        let expr = parse_bracket_expr("[[ ABC == DEF ]]").unwrap();
        assert_eq!(expr.left, "ABC");
        assert_eq!(expr.op, "==");
        assert_eq!(expr.right, "DEF");
    }

    #[test]
    fn parse_double_bracket_inequality() {
        let expr = parse_bracket_expr("[[ foo != bar ]]").unwrap();
        assert_eq!(expr.left, "foo");
        assert_eq!(expr.op, "!=");
        assert_eq!(expr.right, "bar");
    }

    fn diff_of(left: &str, op: &str, right: &str, palette: Palette) -> Vec<String> {
        let expr = BracketExpr {
            left: left.into(),
            op: op.into(),
            right: right.into(),
        };
        bracket_diff_lines(&expr, palette)
    }

    #[test]
    fn plain_diff_points_at_the_characters_that_differ() {
        // Off a terminal there is no colour to show the change, so the diff
        // would otherwise repeat `left`/`right` verbatim.
        assert_eq!(
            diff_of("hash4  -", "=", "hash5  -", Palette::PLAIN),
            [
                "  left: \"hash4  -\"",
                " right: \"hash5  -\"",
                "  diff: \"hash4  -\"",
                "             ^",
                "        \"hash5  -\"",
                "             ^",
            ]
        );
    }

    #[test]
    fn plain_diff_marks_only_the_side_with_something_of_its_own() {
        // `right` is a prefix of `left`: everything that differs is on the
        // left, so the right line gets no caret line under it.
        assert_eq!(
            diff_of("abc ", "==", "abc", Palette::PLAIN),
            [
                "  left: \"abc \"",
                " right: \"abc\"",
                "  diff: \"abc \"",
                "            ^",
                "        \"abc\"",
            ]
        );
    }

    #[test]
    fn plain_diff_carets_follow_display_width() {
        // A wide character takes two columns, so the caret after it has to
        // move over by two to stay under the character that changed.
        let lines = diff_of("日x", "=", "日y", Palette::PLAIN);
        assert_eq!(lines[3], "           ^");
        assert_eq!(lines[5], "           ^");
    }

    #[test]
    fn colored_diff_has_no_caret_lines() {
        let lines = diff_of("ab", "=", "ac", Palette::ANSI);
        assert_eq!(
            lines,
            [
                "  left: \"ab\"",
                " right: \"ac\"",
                "  diff: \"a\x1b[31mb\x1b[0m\"",
                "        \"a\x1b[32mc\x1b[0m\"",
            ]
        );
    }

    #[test]
    fn numeric_comparison_has_no_diff() {
        assert_eq!(diff_of("1", "-eq", "2", Palette::PLAIN).len(), 2);
    }

    #[test]
    fn parse_bracket_not_a_bracket() {
        assert!(parse_bracket_expr("echo hello").is_none());
    }

    #[test]
    fn parse_bracket_operands_with_spaces() {
        // `[ "$a" = "$b" ]` over multi-word values: xtrace quotes each operand,
        // so the words inside them must not be mistaken for separate arguments.
        let expr = parse_bracket_expr("'[' 'apple banana cherry' = 'apple banana durian' ']'")
            .expect("quoted operands should parse");
        assert_eq!(expr.left, "apple banana cherry");
        assert_eq!(expr.op, "=");
        assert_eq!(expr.right, "apple banana durian");
    }

    #[test]
    fn parse_bracket_operand_with_embedded_quote() {
        // A `'` inside a quoted word is spelled `'\''` by xtrace.
        let expr = parse_bracket_expr(r"'[' 'it'\''s here' = \' ']'").unwrap();
        assert_eq!(expr.left, "it's here");
        assert_eq!(expr.right, "'");
    }

    #[test]
    fn parse_bracket_empty_operand_is_unquoted() {
        // `[ foo = "" ]` renders the empty operand as `''`; the quotes belong to
        // xtrace, not to the value.
        let expr = parse_bracket_expr("'[' foo = '' ']'").unwrap();
        assert_eq!(expr.left, "foo");
        assert_eq!(expr.right, "");
    }

    #[test]
    fn parse_bracket_operand_with_control_chars() {
        // Comparing captured multi-line output renders as `$'...'`.
        let expr = parse_bracket_expr(r"'[' $'line3\nline2' = $'a\tb\x21' ']'").unwrap();
        assert_eq!(expr.left, "line3\nline2");
        assert_eq!(expr.right, "a\tb!");
    }

    #[test]
    fn parse_bracket_operand_containing_operator_text() {
        // `=` as part of a value must not be taken for the comparison operator.
        let expr = parse_bracket_expr("'[' a=b = 'c = d' ']'").unwrap();
        assert_eq!(expr.left, "a=b");
        assert_eq!(expr.op, "=");
        assert_eq!(expr.right, "c = d");
    }

    #[test]
    fn parse_bracket_rejects_unterminated_quote() {
        assert!(parse_bracket_expr("'[' 'unterminated = x ']'").is_none());
    }

    #[test]
    fn parse_test_builtin_equality() {
        // `test "A" = "B"` is the same builtin as `[ "A" = "B" ]` and quotes
        // its operands the same way, so it gets the same operands back.
        let expr = parse_bracket_expr("test ABC = DEF").expect("test builtin should parse");
        assert_eq!(expr.left, "ABC");
        assert_eq!(expr.op, "=");
        assert_eq!(expr.right, "DEF");
    }

    #[test]
    fn parse_test_builtin_numeric() {
        let expr = parse_bracket_expr("test 1 -eq 2").unwrap();
        assert_eq!(expr.left, "1");
        assert_eq!(expr.op, "-eq");
        assert_eq!(expr.right, "2");
    }

    #[test]
    fn parse_test_builtin_operands_with_spaces() {
        let expr = parse_bracket_expr("test 'hello world' = 'hello worlx'").unwrap();
        assert_eq!(expr.left, "hello world");
        assert_eq!(expr.op, "=");
        assert_eq!(expr.right, "hello worlx");
    }

    #[test]
    fn parse_test_builtin_unary_declines() {
        // A unary test has no two operands to compare.
        assert!(parse_bracket_expr("test -f /tmp/x").is_none());
        assert!(parse_bracket_expr("test ''").is_none());
    }

    #[test]
    fn parse_test_builtin_needs_the_whole_word() {
        // A command whose name merely starts with `test` is not the builtin:
        // the test functions themselves are named that way, and the first
        // trace line of every run is the runner calling one of them.
        assert!(parse_bracket_expr("testEquality").is_none());
        assert!(parse_bracket_expr("tester a = b").is_none());
    }

    #[test]
    fn parse_double_bracket_operands_with_spaces() {
        // `[[ ]]` is shell syntax, so bash prints its operands unquoted; the
        // operator token is the only boundary available.
        let expr = parse_bracket_expr("[[ apple banana cherry = apple banana durian ]]").unwrap();
        assert_eq!(expr.left, "apple banana cherry");
        assert_eq!(expr.op, "=");
        assert_eq!(expr.right, "apple banana durian");
    }

    #[test]
    fn parse_double_bracket_ambiguous_split_declines() {
        // Two candidate operators: we can't tell which one bash meant.
        assert!(parse_bracket_expr("[[ a = b = c ]]").is_none());
    }

    #[test]
    fn escape_for_display_keeps_values_on_one_line() {
        assert_eq!(escape_for_display("a\nb\tc"), r"a\nb\tc");
        assert_eq!(escape_for_display(r"back\slash"), r"back\\slash");
        assert_eq!(escape_for_display("quote\"d"), r#"quote\"d"#);
        assert_eq!(escape_for_display("bell\x07"), r"bell\x07");
        assert_eq!(escape_for_display("plain"), "plain");
    }

    /// A parsed file with the given script and function bounds.
    fn test_file(script: &str, functions: Vec<(usize, usize)>) -> TestFile {
        TestFile {
            path: "x.test".into(),
            canonical_path: "/x.test".into(),
            tests: vec!["test_x".into()],
            script: script.into(),
            functions,
            shell: "bash".into(),
            ..Default::default()
        }
    }

    /// A file whose one function spans lines 1 through `lines`, so a trace
    /// line numbered anywhere in that range reads as the test's own.
    fn one_function(lines: usize) -> TestFile {
        let script = format!("test_x() {{\n{}}}\n", ":\n".repeat(lines - 2));
        test_file(&script, vec![(0, lines - 1)])
    }

    /// A log written to a fresh context dir, parsed against `file`.
    fn failure_of(log: &str, file: &TestFile) -> Option<FailureInfo> {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("xtrace.log"), log).unwrap();
        parse_xtrace_failure(tmp.path(), file)
    }

    #[test]
    fn parse_xtrace_finds_last_command() {
        let info = failure_of(
            "+3: echo hello\n+4: echo world\n+5: '[' ABC = DEF ']'\n",
            &one_function(10),
        )
        .unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.command, "'[' ABC = DEF ']'");
        assert_eq!(info.function, (0, 9));
    }

    #[test]
    fn parse_xtrace_skips_subshell() {
        let info = failure_of(
            "+3: echo hello\n++4: subshell_cmd\n+5: false\n",
            &one_function(10),
        )
        .unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.command, "false");
    }

    #[test]
    fn parse_xtrace_skips_the_runners_own_call() {
        // The first trace line is the runner invoking the test, numbered past
        // the end of the definitions. It is no command of the test's, and it
        // must not end the scan before the test has run anything either.
        let info = failure_of("+40: test_x\n+5: false\n", &one_function(10)).unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.command, "false");
    }

    #[test]
    fn parse_xtrace_stops_at_an_exit_trap_action() {
        // `trap 'rm -rf "$tmp"' EXIT`, the usual cleanup idiom: the shell runs
        // the trap string after the failure with `$LINENO` restarted at 1, so
        // its commands trace as lines of the file's header. The failing
        // command is the last one the body ran before the trap fired, not the
        // trap's `rm` — and not line 1 of the file, which used to be the
        // snippet's answer.
        let file = test_file(
            "\n\ntest_x() {\n  trap 'rm -rf x' EXIT\n  '[' a = b ']'\n}\n",
            vec![(2, 5)],
        );
        let log = "+20: test_x\n+4: trap 'rm -rf x' EXIT\n+5: '[' a = b ']'\n+1: rm -rf x\n";
        let info = failure_of(log, &file).unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.command, "'[' a = b ']'");
        assert_eq!(info.function, (2, 5));
    }

    #[test]
    fn parse_xtrace_stops_at_an_exit_trap_calling_a_function() {
        // `trap cleanup EXIT`: the action itself traces outside every function,
        // but the function it calls is in the script, so its lines would read
        // as the test's own if the scan carried on past the action. The body
        // was over the moment the trap fired.
        let file = test_file(
            "\ncleanup() {\n  rm -f marker\n}\ntest_x() {\n  trap cleanup EXIT\n  false\n}\n",
            vec![(1, 3), (4, 7)],
        );
        let log = "+20: test_x\n+6: trap cleanup EXIT\n+7: false\n+1: cleanup\n+3: rm -f marker\n";
        let info = failure_of(log, &file).unwrap();
        assert_eq!(info.lineno, 7);
        assert_eq!(info.command, "false");
        assert_eq!(info.function, (4, 7));
    }

    #[test]
    fn parse_xtrace_stops_at_an_exit_trap_when_line_1_opens_a_function() {
        // No shebang, so line 1 is another test's header and the trap's `+1:`
        // falls inside a function. It is that function's header, which no body
        // traces, so the trap still ends the scan — rather than the snippet
        // landing on `test_a`, which never ran.
        let file = TestFile {
            headers: vec![0, 3],
            ..test_file(
                "test_a() {\n  sleep 5\n}\ntest_x() {\n  trap 'echo bye' EXIT\n  false\n}\n",
                vec![(0, 2), (3, 6)],
            )
        };
        let log = "+20: test_x\n+5: trap 'echo bye' EXIT\n+6: false\n+1: echo bye\nbye\n";
        let info = failure_of(log, &file).unwrap();
        assert_eq!(info.lineno, 6);
        assert_eq!(info.command, "false");
        assert_eq!(info.function, (3, 6));

        // Under `set -u` the action may fail in expansion instead, and the
        // shell's error names line 1: not the test's line either.
        let log = "+20: test_x\n+6: false\n/x.test: line 1: NOPE: unbound variable\n";
        assert_eq!(failure_of(log, &file).unwrap().lineno, 6);
    }

    #[test]
    fn parse_xtrace_declines_when_no_command_of_the_test_ran() {
        // Nothing to point at: the only trace line is the runner's call.
        assert!(failure_of("+40: test_x\n", &one_function(10)).is_none());
    }

    #[test]
    fn parse_xtrace_blames_the_line_the_shell_gave_up_on() {
        // `set -u` and an unbound variable: the shell stops in expansion,
        // before it traces the command, so the last trace line is the command
        // *before* it — which succeeded. The error the shell prints names the
        // right line, and that is where the snippet has to point.
        let log = "+40: test_x\n+3: set -u\n+4: echo before\n\
                   /x.test: line 5: NOPE: unbound variable\n";
        let info = failure_of(log, &one_function(10)).unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.command, "");
        assert_eq!(info.function, (0, 9));
    }

    #[test]
    fn parse_xtrace_blames_the_shells_line_inside_a_helper() {
        // The line the shell names may be in another function than the last
        // trace line — a helper the test called, whose `${x:?}` failed. The
        // snippet's window is the function of the line it points at.
        let file = test_file(
            "\nhelper() {\n  echo in\n  echo \"${REQ:?}\"\n}\ntest_x() {\n  helper\n}\n",
            vec![(1, 4), (5, 7)],
        );
        let log = "+20: test_x\n+7: helper\n+3: echo in\n/x.test: line 4: REQ: parameter null or not set\n";
        let info = failure_of(log, &file).unwrap();
        assert_eq!(info.lineno, 4);
        assert_eq!(info.function, (1, 4));
    }

    #[test]
    fn parse_xtrace_lets_a_later_trace_line_outrank_an_earlier_error() {
        // A message the shell printed and carried on from (here a warning on
        // line 3) does not place the failure once a later command has traced:
        // whichever of the two came last is where the test stopped.
        let log = "+40: test_x\n+3: cat\n/x.test: line 3: warning: here-document delimited by end-of-file\n+6: false\n";
        let info = failure_of(log, &one_function(10)).unwrap();
        assert_eq!(info.lineno, 6);
        assert_eq!(info.command, "false");
    }

    #[test]
    fn parse_xtrace_stays_on_the_body_when_an_exit_trap_follows_the_shells_error() {
        // The shell's error, then the trap running with `$LINENO` restarted:
        // the error's line is the failure, and the `+1:` still ends the scan.
        let file = test_file(
            "\n\ntest_x() {\n  trap 'rm -rf x' EXIT\n  echo \"$NOPE\"\n}\n",
            vec![(2, 5)],
        );
        let log = "+20: test_x\n+4: trap 'rm -rf x' EXIT\n/x.test: line 5: NOPE: unbound variable\n+1: rm -rf x\n";
        let info = failure_of(log, &file).unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.function, (2, 5));
    }

    #[test]
    fn parse_xtrace_ignores_another_scripts_error_and_a_line_outside_the_file() {
        // A program the test ran may print the same shape of message about
        // *its* lines (`bash other.sh`), and the shell itself may name a line
        // the script has no function at — neither says where this test is.
        let log = "+40: test_x\n+4: bash other.sh\n\
                   other.sh: line 2: X: unbound variable\n\
                   /x.test: line 30: Y: unbound variable\n";
        let info = failure_of(log, &one_function(10)).unwrap();
        assert_eq!(info.lineno, 4);
        assert_eq!(info.command, "bash other.sh");
    }

    #[test]
    fn parse_shell_error_line_reads_bash_and_dash_spellings() {
        let script = Path::new("/repo/t.test");
        let parse = |line: &str| parse_shell_error_line(line.as_bytes(), script);
        assert_eq!(
            parse("/repo/t.test: line 11: X: unbound variable"),
            Some(11)
        );
        assert_eq!(parse("/repo/t.test: 11: X: parameter not set"), Some(11));
        // Trailing `\r` or not, the number is what is read.
        assert_eq!(parse("/repo/t.test: line 7: msg\r"), Some(7));
        // Another path, no number, a number with no message after it, or a
        // number spelled with a sign are not the shell placing a failure.
        assert_eq!(
            parse("/repo/other.test: line 11: X: unbound variable"),
            None
        );
        assert_eq!(parse("/repo/t.test: line : msg"), None);
        assert_eq!(parse("/repo/t.test: line 11"), None);
        assert_eq!(parse("/repo/t.test: line +11: msg"), None);
        assert_eq!(parse("/repo/t.test: some output: 11: x"), None);
        assert_eq!(parse("/repo/t.test.bak: line 3: msg"), None);
    }

    #[test]
    fn parse_xtrace_finds_the_command_behind_a_wall_of_its_own_output() {
        // The shape of a realistic noisy failure: the command that failed
        // printed far more than it traced, so its trace line is a long way
        // from the end of the log and the tail of the file holds no trace at
        // all. The scan has to reach it without holding the whole log, and
        // without holding one of its lines either — the output here has no
        // newlines in it, so it is a single line as far as the scan can tell.
        let mut log = String::from("+3: setup\n+7: noisy_command\n");
        log.push_str(&"x".repeat(MAX_LINE * 3));

        let info = failure_of(&log, &one_function(10)).unwrap();
        assert_eq!(info.lineno, 7);
        assert_eq!(info.command, "noisy_command");
    }

    #[test]
    fn parse_xtrace_resumes_after_an_over_long_line() {
        // An over-long line is truncated, not treated as the end of the log:
        // the trace lines after it still have to be found.
        let mut log = String::from("+3: setup\n");
        log.push_str(&"y".repeat(MAX_LINE * 2));
        log.push_str("\n+9: false\n");

        let info = failure_of(&log, &one_function(10)).unwrap();
        assert_eq!(info.lineno, 9);
        assert_eq!(info.command, "false");
    }

    #[test]
    fn parse_xtrace_keeps_the_line_number_of_an_over_long_trace() {
        // A trace line longer than the cap loses its tail, so no operand diff
        // comes out of it — but the line number is at the front of the line,
        // so the snippet still points at the command that failed.
        let log = format!("+4: '[' {} = b ']'\n", "a".repeat(MAX_LINE * 2));

        let info = failure_of(&log, &one_function(10)).unwrap();
        assert_eq!(info.lineno, 4);
        assert_eq!(info.command.len(), MAX_LINE - "+4: ".len());
        assert!(parse_bracket_expr(&info.command).is_none());
    }

    #[test]
    fn parse_xtrace_reads_the_last_line_without_a_trailing_newline() {
        let info = failure_of("+3: echo hi\n+4: false", &one_function(10)).unwrap();
        assert_eq!(info.lineno, 4);
        assert_eq!(info.command, "false");
    }

    #[test]
    fn snippet_defuses_c1_controls_in_the_source_it_renders() {
        // A script may hold a C1 control anywhere, and the snippet prints the
        // lines around the failure. `annotate_snippets` substitutes the C0
        // controls it renders but not these, and a terminal in UTF-8 mode
        // obeys them — `U+009B` is `CSI` — so the file would get to redraw the
        // report blaming it.
        let lines = [
            "test_sneak() {",
            "  : \"\u{9b}2A\u{9b}2K\u{9b}32mPASS\u{9b}0m\"",
            "  false",
            "}",
        ];
        let rendered = render_snippet(
            "command failed",
            Path::new("t.sh"),
            &lines,
            Highlight::line(2),
            0..4,
            true,
        );

        assert!(
            !rendered.contains('\u{9b}'),
            "a C1 control reached the terminal: {rendered:?}"
        );
        assert!(
            rendered.contains(r"\x9b2A\x9b2K\x9b32mPASS\x9b0m"),
            "{rendered}"
        );

        // Escaping widens the line the annotation is measured against, so the
        // span has to be computed after it: the caret still has to land under
        // `false` and nowhere else.
        let underline: Vec<&str> = rendered.lines().filter(|l| l.contains("^^^")).collect();
        assert_eq!(underline.len(), 1, "{rendered}");
        let caret_col = strip_ansi(underline[0]).find('^').unwrap();
        let false_line = rendered
            .lines()
            .find(|l| strip_ansi(l).contains("false"))
            .unwrap();
        assert_eq!(
            strip_ansi(false_line).find("false"),
            Some(caret_col),
            "{rendered}"
        );
        assert_eq!(strip_ansi(underline[0]).trim_end().len() - caret_col, 5);
    }

    #[test]
    fn snippet_names_the_file_by_its_discovered_path() {
        // Two files with one base name in different directories have to be
        // told apart, and the location has to resolve from where the run was
        // started — so the path is the one the walk found, minus the `./`
        // it walked from, as `list` prints it.
        let lines = ["test_x() {", "  false", "}"];
        let render = |path: &str| {
            render_snippet(
                "command failed",
                Path::new(path),
                &lines,
                Highlight::line(1),
                0..3,
                false,
            )
        };
        assert!(render("./x/t.test").contains("--> x/t.test:2:3"));
        assert!(render("../y/t.test").contains("--> ../y/t.test:2:3"));
        assert!(render("/abs/t.test").contains("--> /abs/t.test:2:3"));
    }

    #[test]
    fn violation_snippet_underlines_the_one_condition_and_labels_it() {
        // Two conditions on one line: the caret has to sit under the one that
        // failed, with what was measured as its label, and the window runs
        // down to the function so the test's name is in view.
        let lines = [
            "#!/bin/sh",
            "",
            "#attest(cpu < 10ms, pids <= 3)",
            "test_x() {",
            "  true",
            "}",
        ];
        let span = lines[2].find("pids <= 3").unwrap();
        let rendered = render_snippet(
            "assertion on the run failed",
            Path::new("t.sh"),
            &lines,
            Highlight {
                line: 2,
                span: Some(span..span + "pids <= 3".len()),
                label: Some("pids was 5"),
            },
            violation_window(&[(3, 5)], 2, lines.len()),
            false,
        );
        assert!(rendered.contains("pids was 5"), "{rendered}");
        assert!(rendered.contains("test_x() {"), "{rendered}");
        assert!(!rendered.contains("#!/bin/sh"), "{rendered}");
        let underline = rendered.lines().find(|l| l.contains("^^^")).unwrap();
        let caret_col = underline.find('^').unwrap();
        let cond_line = rendered.lines().find(|l| l.contains("#attest(")).unwrap();
        assert_eq!(cond_line.find("pids <= 3"), Some(caret_col), "{rendered}");
        assert_eq!(
            underline
                .trim_end()
                .strip_suffix(" pids was 5")
                .map(str::len),
            Some(caret_col + "pids <= 3".len()),
            "{rendered}"
        );
    }

    #[test]
    fn violation_window_reaches_a_test_further_down_than_the_context() {
        // The README's own multi-line form: the first condition sits four
        // lines above the definition, past the context a failed command gets,
        // and the snippet used to stop on the doc comment without ever naming
        // the test.
        let lines = [
            "#!/bin/sh",
            "# attest(",
            "#   2ms < cpu < 10ms,",
            "#   pids <= 3,",
            "# )",
            "## Doc comments may still sit between the block and the test.",
            "testHello() {",
            "  true",
            "  true",
            "}",
            "testOther() {",
            "  true",
            "}",
        ];
        let functions = [(6, 9), (10, 12)];
        assert_eq!(violation_window(&functions, 2, lines.len()), 2..8);
        let rendered = render_snippet(
            "assertion on the run failed",
            Path::new("t.sh"),
            &lines,
            Highlight {
                line: 2,
                span: Some(4..20),
                label: Some("cpu was 12.2ms"),
            },
            violation_window(&functions, 2, lines.len()),
            false,
        );
        assert!(rendered.contains("testHello() {"), "{rendered}");

        // Close by, the usual context still applies, and nothing reaches
        // past the decorated function into the next one.
        assert_eq!(violation_window(&functions, 4, lines.len()), 4..8);
        assert_eq!(violation_window(&functions, 5, lines.len()), 5..9);
        assert_eq!(violation_window(&[(6, 7), (8, 9)], 5, 10), 5..8);
    }

    #[test]
    fn a_violation_report_defuses_the_paths_a_test_wrote() {
        // A `writes` violation names paths the test created, and a file name
        // is any bytes but `/` and NUL: one spelled with `ESC [2K` and `CSI`
        // must reach the terminal as text, both in the snippet's label and
        // in the full list of offenders under it.
        let erase = "/tmp/a\x1b[2K\u{9b}1Gb";
        let offenders: Vec<String> = (1..=4).map(|i| format!("{erase}{i}")).collect();
        let message = format!("wrote {} (+3 more)", offenders[0]);
        let file = test_file(
            "#!/bin/bash\n# attest(writes in {./ok})\ntest_x() {\n    :\n}\n",
            vec![(2, 4)],
        );
        let violation = crate::decorator::Violation {
            line: 2,
            span: 9..26,
            condition: "writes in {./ok}".into(),
            message,
            detail: crate::decorator::Detail::Set { offenders },
        };
        let result = TestResult {
            name: "test_x".into(),
            test: "test_x",
            file: &file,
            outcome: Outcome::Fail,
            duration: std::time::Duration::ZERO,
            context: std::path::PathBuf::new(),
            resources: None,
            syscalls: None,
            violations: vec![violation],
        };

        let hostile = |c: char| c.is_control() && c != '\n';
        let report = violations_report(&result, false);
        assert!(
            !report.contains(hostile),
            "a control character reached the report:\n{report:?}"
        );
        assert!(report.contains(r"wrote /tmp/a\x1b[2K\x9b1Gb1 (+3 more)"));
        for i in 1..=4 {
            assert!(report.contains(&format!(r"  /tmp/a\x1b[2K\x9b1Gb{i}")));
        }

        // The fallback for a condition the script has no line for is
        // escaped the same way.
        let mut result = result;
        result.violations[0].line = 99;
        let report = violations_report(&result, false);
        assert!(!report.contains(hostile), "{report:?}");
    }

    /// Drop the SGR sequences `Renderer::styled` wraps the snippet in, so a
    /// column in the rendered line is a column on screen.
    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            // `ESC [ ... m`: skip to the final byte of the sequence.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }
}

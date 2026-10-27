use std::path::Path;

use crate::output::{Palette, out_palette, outln};
use crate::parser::TestFile;
use crate::runner::{Outcome, TestResult};

/// Information extracted from an xtrace log about the failing command.
struct FailureInfo {
    /// 1-based line number where the failure occurred. The runner script
    /// preserves the source's line numbers, so this is also the line in the
    /// source file.
    lineno: usize,
    /// The command text as shown in xtrace (e.g. `'[' ABC = DEF ']'`).
    command: String,
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
pub fn print_failure_snippet(result: &TestResult, file: &TestFile) {
    let Some(failure) = parse_xtrace_failure(&result.context) else {
        return;
    };

    let lines: Vec<&str> = file.script.lines().collect();
    let Some(line_idx) = failure.lineno.checked_sub(1) else {
        return;
    };
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
    let (func_start_line, func_end_line) = file
        .enclosing_function(line_idx)
        .unwrap_or((line_idx, line_idx));

    outln!(
        "{}",
        render_snippet(
            title,
            &result.source_path,
            &lines,
            line_idx,
            func_start_line,
            func_end_line,
            out_palette().is_colored(),
        )
    );

    // If the failing command is a `[` expression, show operand details
    if let Some(expr) = parse_bracket_expr(&failure.command) {
        render_bracket_diff(&expr);
    }
}

/// Parse the xtrace log to find the last executed command (which is the one that failed).
fn parse_xtrace_failure(tmp_dir: &Path) -> Option<FailureInfo> {
    // Open the log the runner created for the test, never a symlink the test
    // left at that name (see `overlay::open_nofollow`).
    let file = crate::overlay::open_nofollow(&tmp_dir.join("xtrace.log")).ok()?;
    let mut reader = std::io::BufReader::new(file);

    // Walk it a line at a time rather than reading it whole: the log holds
    // everything the test and the programs it ran sent to stderr, so it is as
    // big as the test chose to make it, and the failing command's own output
    // sits between its trace line and the end of the file — a tail would miss
    // exactly the command a noisy failure needs attributing to.
    let mut last_match: Option<FailureInfo> = None;
    let mut line = Vec::new();
    while read_capped_line(&mut reader, &mut line).unwrap_or(false) {
        // Lossy because a test is free to write non-UTF-8 to its stderr, and a
        // diagnostic is better than none.
        if let Some(info) = parse_trace_line(&String::from_utf8_lossy(&line)) {
            last_match = Some(info);
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
/// `BufRead::read_until` would be this function, minus the cap: it grows the
/// buffer to hold the whole line however long that is.
fn read_capped_line(
    reader: &mut impl std::io::BufRead,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    line.clear();
    let mut any = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(any);
        }
        any = true;
        let newline = buf.iter().position(|&b| b == b'\n');
        let len = newline.unwrap_or(buf.len());
        let room = MAX_LINE.saturating_sub(line.len());
        line.extend_from_slice(&buf[..room.min(len)]);
        // The newline itself is consumed with the line it ends but kept out of
        // it, the way `str::lines` splits.
        reader.consume(len + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(true);
        }
    }
}

/// One line of an xtrace log as a [`FailureInfo`], if it traces a command at
/// the top level: `+<lineno>: <command>` in the runner's `PS4` format.
/// Subshell traces (`++`, `+++`, …) and anything the test printed itself are
/// not commands this failure can be attributed to.
fn parse_trace_line(line: &str) -> Option<FailureInfo> {
    // A `\r` before the newline belongs to the terminal conventions of
    // whatever wrote the line, not to the trace (`str::lines` drops it too).
    let rest = line.strip_suffix('\r').unwrap_or(line).strip_prefix('+')?;
    if rest.starts_with('+') {
        return None;
    }
    let (lineno, command) = rest.split_once(": ")?;
    Some(FailureInfo {
        lineno: lineno.trim().parse().ok()?,
        command: command.to_string(),
    })
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

/// Render an annotate-snippets diagnostic highlighting `lines[line_idx]`, with
/// up to [`CONTEXT_LINES`] lines of surrounding context on each side, clamped
/// to the enclosing function so a snippet never leaks into an adjacent test.
fn render_snippet(
    title: &str,
    source_path: &Path,
    lines: &[&str],
    line_idx: usize,
    func_start_line: usize,
    func_end_line: usize,
    colored: bool,
) -> String {
    use annotate_snippets::{AnnotationKind, Level, Renderer, Snippet};

    // A file name comes out of the directory walk, so it is as much the tree's
    // choice as a test name is: annotate-snippets substitutes the control
    // characters in the rendered *source*, but prints the path it is given
    // as-is.
    let file_name = source_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| source_path.to_string_lossy());
    let path_str = crate::output::escape_label(&file_name);

    let start = line_idx.saturating_sub(CONTEXT_LINES).max(func_start_line);
    let end = (line_idx + CONTEXT_LINES + 1)
        .min(lines.len())
        .min(func_end_line + 1);
    // Escaped before anything is measured, so every offset below is an offset
    // into the text that actually gets rendered (see `escape_c1`).
    let window: Vec<std::borrow::Cow<str>> =
        lines[start..end].iter().copied().map(escape_c1).collect();
    let source = window.join("\n");

    // Annotate the whole failing line within the rendered window, skipping its
    // leading indentation. `join("\n")` puts one byte between lines.
    let line_start: usize = window[..line_idx - start].iter().map(|l| l.len() + 1).sum();
    let failing = &*window[line_idx - start];
    let indent = failing.len() - failing.trim_start().len();

    let report = &[Level::ERROR.primary_title(title).element(
        Snippet::source(&source)
            .path(&*path_str)
            .line_start(start + 1)
            .fold(false)
            .annotation(
                AnnotationKind::Primary.span(line_start + indent..line_start + failing.len()),
            ),
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
    let Palette {
        green, red, reset, ..
    } = out_palette();
    let left = escape_for_display(&expr.left);
    let right = escape_for_display(&expr.right);

    outln!();
    outln!("  left: \"{left}\"");
    outln!(" right: \"{right}\"");

    // For equality operators, show inline diff if values differ
    if matches!(expr.op.as_str(), "=" | "==" | "!=") && left != right {
        use similar::{ChangeTag, TextDiff};
        let diff = TextDiff::from_chars(&left, &right);
        let mut left_hl = String::new();
        let mut right_hl = String::new();
        for change in diff.iter_all_changes() {
            let val = change.value();
            match change.tag() {
                ChangeTag::Equal => {
                    left_hl.push_str(val);
                    right_hl.push_str(val);
                }
                ChangeTag::Delete => {
                    left_hl.push_str(red);
                    left_hl.push_str(val);
                    left_hl.push_str(reset);
                }
                ChangeTag::Insert => {
                    right_hl.push_str(green);
                    right_hl.push_str(val);
                    right_hl.push_str(reset);
                }
            }
        }
        outln!("  diff: \"{left_hl}\"");
        outln!("        \"{right_hl}\"");
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

    #[test]
    fn parse_xtrace_finds_last_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let xtrace = tmp.path().join("xtrace.log");
        std::fs::write(
            &xtrace,
            "+3: echo hello\n+4: echo world\n+5: '[' ABC = DEF ']'\n",
        )
        .unwrap();

        let info = parse_xtrace_failure(tmp.path()).unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.command, "'[' ABC = DEF ']'");
    }

    #[test]
    fn parse_xtrace_skips_subshell() {
        let tmp = tempfile::TempDir::new().unwrap();
        let xtrace = tmp.path().join("xtrace.log");
        std::fs::write(&xtrace, "+3: echo hello\n++4: subshell_cmd\n+5: false\n").unwrap();

        let info = parse_xtrace_failure(tmp.path()).unwrap();
        assert_eq!(info.lineno, 5);
        assert_eq!(info.command, "false");
    }

    #[test]
    fn parse_xtrace_finds_the_command_behind_a_wall_of_its_own_output() {
        // The shape of a realistic noisy failure: the command that failed
        // printed far more than it traced, so its trace line is a long way
        // from the end of the log and the tail of the file holds no trace at
        // all. The scan has to reach it without holding the whole log, and
        // without holding one of its lines either — the output here has no
        // newlines in it, so it is a single line as far as the scan can tell.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut log = String::from("+3: setup\n+7: noisy_command\n");
        log.push_str(&"x".repeat(MAX_LINE * 3));
        std::fs::write(tmp.path().join("xtrace.log"), &log).unwrap();

        let info = parse_xtrace_failure(tmp.path()).unwrap();
        assert_eq!(info.lineno, 7);
        assert_eq!(info.command, "noisy_command");
    }

    #[test]
    fn parse_xtrace_resumes_after_an_over_long_line() {
        // An over-long line is truncated, not treated as the end of the log:
        // the trace lines after it still have to be found.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut log = String::from("+3: setup\n");
        log.push_str(&"y".repeat(MAX_LINE * 2));
        log.push_str("\n+9: false\n");
        std::fs::write(tmp.path().join("xtrace.log"), &log).unwrap();

        let info = parse_xtrace_failure(tmp.path()).unwrap();
        assert_eq!(info.lineno, 9);
        assert_eq!(info.command, "false");
    }

    #[test]
    fn parse_xtrace_keeps_the_line_number_of_an_over_long_trace() {
        // A trace line longer than the cap loses its tail, so no operand diff
        // comes out of it — but the line number is at the front of the line,
        // so the snippet still points at the command that failed.
        let tmp = tempfile::TempDir::new().unwrap();
        let log = format!("+4: '[' {} = b ']'\n", "a".repeat(MAX_LINE * 2));
        std::fs::write(tmp.path().join("xtrace.log"), &log).unwrap();

        let info = parse_xtrace_failure(tmp.path()).unwrap();
        assert_eq!(info.lineno, 4);
        assert_eq!(info.command.len(), MAX_LINE - "+4: ".len());
        assert!(parse_bracket_expr(&info.command).is_none());
    }

    #[test]
    fn parse_xtrace_reads_the_last_line_without_a_trailing_newline() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("xtrace.log"), "+3: echo hi\n+4: false").unwrap();

        let info = parse_xtrace_failure(tmp.path()).unwrap();
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
        let rendered = render_snippet("command failed", Path::new("t.sh"), &lines, 2, 0, 3, true);

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

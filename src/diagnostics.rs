use std::path::Path;

use crate::output::{GREEN, RED, RESET};
use crate::runner::TestResult;

/// Information extracted from an xtrace log about the failing command.
struct FailureInfo {
    /// 1-based line number where the failure occurred. `functions.sh` preserves
    /// the source's line numbers, so this is also the line in the source file.
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

/// Print a source snippet showing where a failed test went wrong.
pub fn print_failure_snippet(result: &TestResult) {
    let Some(failure) = parse_xtrace_failure(&result.context) else {
        return;
    };

    // The runner emits `functions.sh` so that each function keeps its original
    // source line numbers, so the xtrace line number (bash's `$LINENO`) indexes
    // the original source directly — no text matching required.
    let Ok(source) = std::fs::read_to_string(&result.source_path) else {
        return;
    };
    let lines: Vec<&str> = source.lines().collect();
    let Some(line_idx) = failure.lineno.checked_sub(1) else {
        return;
    };
    if line_idx >= lines.len() {
        return;
    }

    // A timed-out test did not fail a command; the highlighted line is simply
    // where it was still running when the clock ran out.
    let title = if result.timed_out {
        "test timed out"
    } else {
        "command failed"
    };

    // Clamp the rendered context to the enclosing function so a snippet never
    // leaks into an adjacent test.
    let (func_start_line, func_end_line) =
        enclosing_function_bounds(&lines, line_idx).unwrap_or((line_idx, line_idx));

    render_snippet(
        title,
        &result.source_path,
        &lines,
        line_idx,
        func_start_line,
        func_end_line,
    );

    // If the failing command is a `[` expression, show operand details
    if let Some(expr) = parse_bracket_expr(&failure.command) {
        render_bracket_diff(&expr);
    }
}

/// Parse the xtrace log to find the last executed command (which is the one that failed).
fn parse_xtrace_failure(tmp_dir: &Path) -> Option<FailureInfo> {
    let xtrace_path = tmp_dir.join("xtrace.log");
    let content = std::fs::read_to_string(xtrace_path).ok()?;

    // Find the last line starting with `+LINENO: ` (our custom PS4 format).
    // Skip lines with `++ ` prefix (subshell traces) and non-trace lines.
    let mut last_match: Option<FailureInfo> = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix('+') {
            // Skip subshell traces (++, +++, etc.)
            if rest.starts_with('+') {
                continue;
            }
            if let Some((lineno_str, command)) = rest.split_once(": ")
                && let Ok(lineno) = lineno_str.trim().parse::<usize>()
            {
                last_match = Some(FailureInfo {
                    lineno,
                    command: command.to_string(),
                });
            }
        }
    }
    last_match
}

/// 0-based line range `(start, end)` of the function definition enclosing
/// `target_line` (0-based), or `None` if the line is not inside a function.
/// Used to clamp the rendered context window. Brace counting is naive (it does
/// not account for braces in strings or `${...}`), matching how the rest of the
/// tool scans shell source.
fn enclosing_function_bounds(lines: &[&str], target_line: usize) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < lines.len() {
        if !is_function_header(lines[i].trim()) {
            i += 1;
            continue;
        }

        let start = i;
        let mut depth: i32 = 0;
        let mut opened = false;
        let mut j = i;
        loop {
            let trimmed = lines[j].trim();
            depth += trimmed.matches('{').count() as i32;
            depth -= trimmed.matches('}').count() as i32;
            if depth > 0 {
                opened = true;
            }
            if opened && depth <= 0 {
                break;
            }
            if j + 1 >= lines.len() {
                break;
            }
            j += 1;
        }

        if (start..=j).contains(&target_line) {
            return Some((start, j));
        }
        i = j + 1;
    }
    None
}

/// Whether a trimmed source line opens a shell function definition
/// (`name()`, `name ()`, or `function name`).
fn is_function_header(trimmed: &str) -> bool {
    if let Some(rest) = trimmed.strip_prefix("function ") {
        return rest
            .trim_start()
            .starts_with(|c: char| c.is_alphanumeric() || c == '_');
    }
    let name_len = trimmed
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(0);
    name_len > 0 && trimmed[name_len..].trim_start().starts_with('(')
}

/// How many lines of context to show on each side of the annotated line.
const CONTEXT_LINES: usize = 3;

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
) {
    use annotate_snippets::{AnnotationKind, Level, Renderer, Snippet};

    let path_str = source_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| source_path.to_string_lossy());

    let start = line_idx.saturating_sub(CONTEXT_LINES).max(func_start_line);
    let end = (line_idx + CONTEXT_LINES + 1)
        .min(lines.len())
        .min(func_end_line + 1);
    let window = &lines[start..end];
    let source = window.join("\n");

    // Annotate the whole failing line within the rendered window, skipping its
    // leading indentation. `join("\n")` puts one byte between lines.
    let line_start: usize = window[..line_idx - start].iter().map(|l| l.len() + 1).sum();
    let failing = lines[line_idx];
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

    println!("{}", Renderer::styled().render(report));
}

/// Comparison operators we know how to render a diff for. `==` is how `[[`
/// usually spells `=`, though bash echoes back whichever one was written.
fn is_comparison_op(op: &str) -> bool {
    matches!(
        op,
        "=" | "==" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
    )
}

/// Parse a `[` or `[[` test command from xtrace output.
///
/// Xtrace renders `[ "A" = "B" ]` as `'[' A = B ']'` and `[[ "A" = "B" ]]` as
/// `[[ A = B ]]`. The two forms need different handling: `[` is an ordinary
/// command, so each operand is a separately quoted word, whereas `[[` is shell
/// syntax and its operands are echoed back verbatim with no quoting at all.
fn parse_bracket_expr(command: &str) -> Option<BracketExpr> {
    if let Some(inner) = command
        .strip_prefix("'[' ")
        .and_then(|s| s.strip_suffix(" ']'"))
    {
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
    let left = escape_for_display(&expr.left);
    let right = escape_for_display(&expr.right);

    println!();
    println!("  left: \"{left}\"");
    println!(" right: \"{right}\"");

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
                    left_hl.push_str(RED);
                    left_hl.push_str(val);
                    left_hl.push_str(RESET);
                }
                ChangeTag::Insert => {
                    right_hl.push_str(GREEN);
                    right_hl.push_str(val);
                    right_hl.push_str(RESET);
                }
            }
        }
        println!("  diff: \"{left_hl}\"");
        println!("        \"{right_hl}\"");
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
    fn is_function_header_recognizes_forms() {
        assert!(is_function_header("test_foo() {"));
        assert!(is_function_header("test_foo () {"));
        assert!(is_function_header("function test_foo {"));
        assert!(is_function_header("function test_foo() {"));
        assert!(!is_function_header("echo hello"));
        assert!(!is_function_header("[ 1 = 2 ]"));
        assert!(!is_function_header("(subshell)"));
    }

    #[test]
    fn enclosing_bounds_finds_containing_function() {
        // 0-based lines:            0            1              2   3  4              5            6   7
        let lines: Vec<&str> =
            "helper() {\n  echo setup\n}\n\ntest_foo() {\n  echo hello\n  false\n}\n"
                .lines()
                .collect();
        // Line 6 (`false`) is inside test_foo (lines 4..=7).
        assert_eq!(enclosing_function_bounds(&lines, 6), Some((4, 7)));
        // Line 1 (`echo setup`) is inside helper (lines 0..=2).
        assert_eq!(enclosing_function_bounds(&lines, 1), Some((0, 2)));
        // Line 3 (the blank separator) is inside no function.
        assert_eq!(enclosing_function_bounds(&lines, 3), None);
    }

    #[test]
    fn enclosing_bounds_handles_brace_on_next_line() {
        let lines: Vec<&str> = "test_foo ()\n{\n  false\n}\n".lines().collect();
        assert_eq!(enclosing_function_bounds(&lines, 2), Some((0, 3)));
    }
}

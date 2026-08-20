use std::path::Path;

use crate::runner::TestResult;

/// Information extracted from an xtrace log about the failing command.
struct FailureInfo {
    /// 1-based line number in functions.sh where the failure occurred.
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

    let functions_sh = result.context.join("functions.sh");
    let Ok(functions_source) = std::fs::read_to_string(&functions_sh) else {
        return;
    };

    // Get the failing line text from functions.sh
    let functions_lines: Vec<&str> = functions_source.lines().collect();
    let Some(failing_line) = functions_lines.get(failure.lineno.wrapping_sub(1)) else {
        return;
    };
    // brush-parser's to_string() reformats code (e.g., adds semicolons).
    // Normalize by stripping trailing `;` for matching against original source.
    let failing_line_trimmed = failing_line.trim().trim_end_matches(';');

    // Read original source and find the matching line
    let Ok(original_source) = std::fs::read_to_string(&result.source_path) else {
        return;
    };

    if let Some(match_info) =
        find_line_in_source(&original_source, &result.name, failing_line_trimmed)
    {
        render_snippet(
            &result.source_path,
            &original_source,
            match_info.byte_start,
            match_info.byte_end,
            match_info.func_start_line,
            match_info.func_end_line,
        );
    }

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

struct SourceMatch {
    byte_start: usize,
    byte_end: usize,
    /// 0-based line where the function definition starts.
    func_start_line: usize,
    /// 0-based line where the function ends (closing brace).
    func_end_line: usize,
}

/// Find a line matching `needle` inside the named function in the original source.
fn find_line_in_source(source: &str, function_name: &str, needle: &str) -> Option<SourceMatch> {
    let mut in_function = false;
    let mut brace_depth: i32 = 0;
    let mut func_start_line: usize = 0;
    // Byte offset of the current line in `source`, tracked incrementally so we
    // don't rescan the whole source on every candidate match.
    let mut line_byte_start: usize = 0;

    for (line_idx, line) in source.lines().enumerate() {
        let byte_start = line_byte_start;
        line_byte_start += line.len() + 1; // +1 for newline
        let trimmed = line.trim();

        if !in_function {
            // Look for `function_name()` or `function_name ()`
            if trimmed.starts_with(function_name)
                && trimmed[function_name.len()..].trim_start().starts_with('(')
            {
                in_function = true;
                func_start_line = line_idx;
                brace_depth += trimmed.matches('{').count() as i32;
                brace_depth -= trimmed.matches('}').count() as i32;
                continue;
            }
        } else {
            brace_depth += trimmed.matches('{').count() as i32;
            brace_depth -= trimmed.matches('}').count() as i32;

            if trimmed.trim_end_matches(';') == needle {
                // Find the trimmed content within the line
                let indent = line.len() - line.trim_start().len();
                let span_start = byte_start + indent;
                let span_end = byte_start + line.len();

                // Find the function end by continuing to scan
                let mut func_end_line = line_idx;
                let mut depth = brace_depth;
                for (i, l) in source.lines().enumerate().skip(line_idx + 1) {
                    let t = l.trim();
                    depth += t.matches('{').count() as i32;
                    depth -= t.matches('}').count() as i32;
                    func_end_line = i;
                    if depth <= 0 {
                        break;
                    }
                }

                return Some(SourceMatch {
                    byte_start: span_start,
                    byte_end: span_end,
                    func_start_line,
                    func_end_line,
                });
            }

            if brace_depth <= 0 {
                in_function = false;
            }
        }
    }
    None
}

/// Render an annotate-snippets diagnostic for the failing line with surrounding context,
/// clamped to the enclosing function boundaries.
fn render_snippet(
    source_path: &Path,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    func_start_line: usize,
    func_end_line: usize,
) {
    use annotate_snippets::{AnnotationKind, Level, Renderer, Snippet};

    let path_str = source_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| source_path.to_string_lossy());

    // Extract a window of context lines around the annotation, clamped to
    // the enclosing function so we never leak into adjacent tests.
    let context_lines = 3;
    let lines: Vec<&str> = source.lines().collect();

    // Find which line the annotation starts on (0-based)
    let anno_line = source[..byte_start].matches('\n').count();

    let window_start = anno_line.saturating_sub(context_lines).max(func_start_line);
    let window_end = (anno_line + context_lines + 1)
        .min(lines.len())
        .min(func_end_line + 1);

    // Byte offset where the window starts in the original source
    let window_byte_start: usize = lines[..window_start]
        .iter()
        .map(|l| l.len() + 1) // +1 for newline
        .sum();

    let window_source: String = lines[window_start..window_end].join("\n");
    let adj_start = byte_start - window_byte_start;
    let adj_end = byte_end - window_byte_start;

    let report = &[Level::ERROR.primary_title("command failed").element(
        Snippet::source(&window_source)
            .path(&*path_str)
            .line_start(window_start + 1)
            .fold(false)
            .annotation(AnnotationKind::Primary.span(adj_start..adj_end)),
    )];

    let renderer = Renderer::styled();
    println!("{}", renderer.render(report));
}

/// Parse a `[` test command from xtrace output.
///
/// Xtrace renders `[ "A" = "B" ]` as `'[' A = B ']'`.
fn parse_bracket_expr(command: &str) -> Option<BracketExpr> {
    let inner = command.strip_prefix("'[' ")?.strip_suffix(" ']'")?;
    let parts: Vec<&str> = inner.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return None;
    }

    let op = parts[1];
    // Only handle comparison operators
    if !matches!(
        op,
        "=" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
    ) {
        return None;
    }

    Some(BracketExpr {
        left: parts[0].to_string(),
        op: op.to_string(),
        right: parts[2].to_string(),
    })
}

/// Render a comparison between bracket expression operands.
fn render_bracket_diff(expr: &BracketExpr) {
    println!();
    println!("  left: \"{}\"", expr.left);
    println!(" right: \"{}\"", expr.right);

    // For equality operators, show inline diff if values differ
    if matches!(expr.op.as_str(), "=" | "!=") && expr.left != expr.right {
        use similar::{ChangeTag, TextDiff};
        let diff = TextDiff::from_chars(&expr.left, &expr.right);
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
                    left_hl.push_str(&format!("\x1b[31m{val}\x1b[0m"));
                }
                ChangeTag::Insert => {
                    right_hl.push_str(&format!("\x1b[32m{val}\x1b[0m"));
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
    fn parse_bracket_not_a_bracket() {
        assert!(parse_bracket_expr("echo hello").is_none());
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
    fn find_line_in_function() {
        let source =
            "helper() {\n  echo setup\n}\n\ntest_foo() {\n  echo hello\n  [ ABC = DEF ]\n}\n";
        let m = find_line_in_source(source, "test_foo", "[ ABC = DEF ]").unwrap();
        assert_eq!(&source[m.byte_start..m.byte_end], "[ ABC = DEF ]");
        assert_eq!(m.func_start_line, 4); // 0-based: "test_foo() {"
        assert_eq!(m.func_end_line, 7); // 0-based: "}"
    }

    #[test]
    fn find_line_not_in_wrong_function() {
        let source = "test_a() {\n  echo hello\n}\n\ntest_b() {\n  echo world\n}\n";
        assert!(find_line_in_source(source, "test_b", "echo hello").is_none());
    }
}

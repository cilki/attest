use std::collections::HashSet;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::Context;
use brush_parser::ast::{
    Command, CompoundCommand, CompoundList, FunctionDefinition, Program, SourceLocation,
};
use brush_parser::{Parser, ParserOptions};
use tracing::warn;

/// File containing tests.
pub struct TestFile {
    /// The file this was parsed from. Every test in it shares it, so it is kept
    /// here rather than repeated per test.
    pub path: PathBuf,
    /// Names of the test functions, in source order — one per name the shell
    /// would actually run (see [`parse_test_file`]).
    pub tests: Vec<String>,
    /// The script the runner writes out and sources to define this file's
    /// functions (see [`build_functions_source`]). It is the same text for
    /// every test in the file, so it is rendered once here instead of per
    /// test: rendering it walks each function's AST to find its source span,
    /// which cost more than reading and writing the file it is built from —
    /// and a file holding 25 tests paid it 25 times over, once more for every
    /// `--repeat`. Keeping the text rather than the AST it came from is also
    /// what lets the parse be thrown away: nothing downstream needs the nodes.
    pub script: String,
}

/// A pattern for selecting tests, parsed from `[<file>/]<name-pattern>`.
///
/// - `file`: if present, the test's file must end with this path
/// - `name`: if present, matches the test function name; `*` is a wildcard;
///   a pattern without `*` matches any name that starts with the pattern
///
/// A leading `./` marks the argument as a path: all of it is the `file` half
/// and `name` is left unconstrained, so `./a/x.test` == `a/x.test/`.
pub struct TestPattern {
    pub file: Option<PathBuf>,
    pub name: Option<String>,
}

impl TestPattern {
    pub fn parse(s: &str) -> Self {
        // A leading `./` marks the argument as a path, so all of it is the file
        // half and there is no name to split off: `./a/x.test` == `a/x.test/`.
        // The marker has to win over the split however many components follow
        // it — honouring it only for a bare name (`./x.test`) made the whole
        // thing collapse back into a file/name split the moment a directory was
        // put in front, which is where a `./` comes from in the first place.
        //
        // Without the marker the last `/` splits file from name, and an
        // argument with no `/` at all is a bare test name. Either side may come
        // out empty (`foo.sh/`, `/test_x`), which means "unconstrained"; a name
        // is attached to a path by spelling the selector out without the `./`
        // (`a/x.test/test_foo`), which is the form `list` prints.
        let (file_part, name_part) = match s.strip_prefix("./") {
            Some(path) => (path, ""),
            None => s.rsplit_once('/').unwrap_or(("", s)),
        };
        Self {
            file: (!file_part.is_empty()).then(|| PathBuf::from(file_part)),
            name: (!name_part.is_empty()).then(|| name_part.to_string()),
        }
    }

    pub fn matches(&self, file: &Path, name: &str) -> bool {
        if let Some(ref file_pat) = self.file
            && !file.ends_with(file_pat)
        {
            return false;
        }
        if let Some(ref name_pat) = self.name
            && !wildcard_match(name_pat, name)
        {
            return false;
        }
        true
    }
}

/// Match `text` against `pattern`.  `*` matches any sequence of characters.
/// A pattern with no `*` is treated as a prefix (implicit trailing `*`).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return text.starts_with(pattern);
    }
    let segments: Vec<&str> = pattern.split('*').collect();
    if !text.starts_with(segments[0]) {
        return false;
    }
    let mut remaining = &text[segments[0].len()..];
    for seg in &segments[1..segments.len() - 1] {
        if seg.is_empty() {
            continue;
        }
        match remaining.find(seg) {
            Some(i) => remaining = &remaining[i + seg.len()..],
            None => return false,
        }
    }
    remaining.ends_with(segments[segments.len() - 1])
}

/// Parse shell source and return every function definition it contains, in
/// source order.
pub(crate) fn parse_functions(source: &str) -> anyhow::Result<Vec<FunctionDefinition>> {
    let reader = BufReader::new(source.as_bytes());
    let options = ParserOptions::default();
    let mut parser = Parser::new(reader, &options);
    let program = parser.parse_program().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(extract_functions(&program))
}

pub fn parse_test_file(path: &Path) -> anyhow::Result<TestFile> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let functions = parse_functions(&contents)
        .map_err(|e| anyhow::anyhow!("parse error in {}: {e}", path.display()))?;

    // A shell function defined more than once in the same file is shadowed:
    // when the runner sources the file and calls the name, only the last
    // definition runs. Emit a single test per name (matching what the shell
    // actually executes) rather than one phantom test per definition — every
    // one of which would run the same, last body. `functions` still carries
    // all definitions so the sourced script is unchanged.
    let mut tests = Vec::new();
    let mut seen = HashSet::new();
    for f in &functions {
        if !f.fname.value.starts_with("test") {
            continue;
        }
        if !seen.insert(f.fname.value.as_str()) {
            warn!(
                "test function `{}` is defined more than once in {}; only the last definition runs",
                f.fname.value,
                path.display()
            );
            continue;
        }
        tests.push(f.fname.value.clone());
    }

    Ok(TestFile {
        path: path.to_path_buf(),
        tests,
        // Built here, from the text this parse came from, so the script and
        // the spans it is cut with can never be out of step: the file is read
        // once and both come from that one read.
        script: build_functions_source(&functions, &contents),
    })
}

/// Build the script that the runner sources to define a file's functions.
///
/// Each function is emitted verbatim at the same line number it occupies in the
/// original source, with the gaps between them blanked out. Because bash's
/// `$LINENO` (used by our xtrace `PS4`) reports the line within the sourced
/// file, keeping functions at their original offsets makes xtrace line numbers
/// match the source exactly — so a failing command's trace points straight at
/// its source line, with no reconstruction or line-mapping heuristics needed on
/// the diagnostics side.
///
/// Only function definitions are emitted (top-level code is dropped), so
/// sourcing the script never runs a test file's top-level statements.
///
/// A function the parser gave no location for — and every function when
/// `source` is empty — falls back to the reformatted AST rendering: line
/// alignment is lost, but the script still defines and runs the same
/// functions.
pub(crate) fn build_functions_source(functions: &[FunctionDefinition], source: &str) -> String {
    let src_lines: Vec<&str> = source.lines().collect();
    let mut out = String::new();
    // 1-based line number of the next line to be written to `out`.
    let mut line = 1usize;

    for func in functions {
        let located = if src_lines.is_empty() {
            None
        } else {
            func.location()
        };
        let Some(span) = located else {
            for l in func.to_string().lines() {
                out.push_str(l);
                out.push('\n');
                line += 1;
            }
            continue;
        };

        // Functions are yielded in source order, so this only pads forward.
        while line < span.start.line {
            out.push('\n');
            line += 1;
        }

        // `end.line` is the line of the closing brace (inclusive here).
        let last = span.end.line.min(src_lines.len());
        for idx in span.start.line..=last {
            if let Some(text) = src_lines.get(idx - 1) {
                out.push_str(text);
            }
            out.push('\n');
            line += 1;
        }
    }

    out
}

fn extract_functions(program: &Program) -> Vec<FunctionDefinition> {
    let mut functions = Vec::new();
    for complete_command in &program.complete_commands {
        extract_from_compound_list(complete_command, &mut functions);
    }
    functions
}

fn extract_from_compound_list(list: &CompoundList, functions: &mut Vec<FunctionDefinition>) {
    for item in &list.0 {
        let and_or = &item.0;
        extract_from_pipeline(&and_or.first, functions);
        for additional in &and_or.additional {
            let pipeline = match additional {
                brush_parser::ast::AndOr::And(p) | brush_parser::ast::AndOr::Or(p) => p,
            };
            extract_from_pipeline(pipeline, functions);
        }
    }
}

fn extract_from_pipeline(
    pipeline: &brush_parser::ast::Pipeline,
    functions: &mut Vec<FunctionDefinition>,
) {
    for cmd in &pipeline.seq {
        match cmd {
            Command::Function(func) => functions.push(func.clone()),
            Command::Compound(compound, _) => extract_from_compound(compound, functions),
            Command::Simple(_) => {}
        }
    }
}

fn extract_from_compound(compound: &CompoundCommand, functions: &mut Vec<FunctionDefinition>) {
    match compound {
        CompoundCommand::BraceGroup(b) => extract_from_compound_list(&b.list, functions),
        CompoundCommand::Subshell(s) => extract_from_compound_list(&s.list, functions),
        CompoundCommand::ForClause(f) => extract_from_compound_list(&f.body.list, functions),
        CompoundCommand::ArithmeticForClause(a) => {
            extract_from_compound_list(&a.body.list, functions)
        }
        CompoundCommand::WhileClause(w) => {
            extract_from_compound_list(&w.0, functions);
            extract_from_compound_list(&w.1.list, functions);
        }
        CompoundCommand::IfClause(i) => {
            extract_from_compound_list(&i.condition, functions);
            extract_from_compound_list(&i.then, functions);
            if let Some(elses) = &i.elses {
                for else_clause in elses {
                    if let Some(cond) = &else_clause.condition {
                        extract_from_compound_list(cond, functions);
                    }
                    extract_from_compound_list(&else_clause.body, functions);
                }
            }
        }
        CompoundCommand::CaseClause(c) => {
            for case_item in &c.cases {
                if let Some(cmd) = &case_item.cmd {
                    extract_from_compound_list(cmd, functions);
                }
            }
        }
        CompoundCommand::UntilClause(w) => {
            extract_from_compound_list(&w.0, functions);
            extract_from_compound_list(&w.1.list, functions);
        }
        CompoundCommand::Arithmetic(_)
        | CompoundCommand::Coprocess(_)
        | CompoundCommand::ExtendedTest(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn pattern_parse_name_only() {
        let p = TestPattern::parse("test_foo");
        assert!(p.file.is_none());
        assert_eq!(p.name.as_deref(), Some("test_foo"));
    }

    #[test]
    fn pattern_parse_file_and_name() {
        let p = TestPattern::parse("foo.sh/test_bar");
        assert_eq!(p.file.as_deref(), Some(Path::new("foo.sh")));
        assert_eq!(p.name.as_deref(), Some("test_bar"));
    }

    #[test]
    fn pattern_parse_file_only() {
        let p = TestPattern::parse("foo.sh/");
        assert_eq!(p.file.as_deref(), Some(Path::new("foo.sh")));
        assert!(p.name.is_none());

        let p = TestPattern::parse("./foo.sh");
        assert_eq!(p.file.as_deref(), Some(Path::new("foo.sh")));
        assert!(p.name.is_none());
    }

    #[test]
    fn pattern_dot_slash_marker_survives_a_directory() {
        // The `./` says "this is a path", so every component of it belongs to
        // the file half — a `./` in front of a multi-component path used to be
        // dropped and the path then split at its last `/`, which asked for a
        // test named `foo.sh` in a file called `tests` and matched nothing.
        let p = TestPattern::parse("./tests/foo.sh");
        assert_eq!(p.file.as_deref(), Some(Path::new("tests/foo.sh")));
        assert!(p.name.is_none());
        assert!(p.matches(Path::new("/repo/tests/foo.sh"), "test_bar"));
        assert!(!p.matches(Path::new("/repo/other/foo.sh"), "test_bar"));

        // Identical to the trailing-slash spelling of the same selector.
        let slash = TestPattern::parse("tests/foo.sh/");
        assert_eq!(p.file, slash.file);
        assert_eq!(p.name, slash.name);
    }

    #[test]
    fn pattern_dot_slash_takes_the_whole_path() {
        // Consequence of the marker winning: a name cannot be appended behind
        // a `./`. It is spelled without the marker instead, which is the form
        // `list` prints.
        let p = TestPattern::parse("./foo.sh/test_bar");
        assert_eq!(p.file.as_deref(), Some(Path::new("foo.sh/test_bar")));
        assert!(p.name.is_none());

        let p = TestPattern::parse("foo.sh/test_bar");
        assert_eq!(p.file.as_deref(), Some(Path::new("foo.sh")));
        assert_eq!(p.name.as_deref(), Some("test_bar"));
    }

    #[test]
    fn pattern_prefix_match() {
        let p = TestPattern::parse("test_foo");
        let file = Path::new("/any/file.sh");
        assert!(p.matches(file, "test_foo"));
        assert!(p.matches(file, "test_foo_bar"));
        assert!(!p.matches(file, "test_baz"));
    }

    #[test]
    fn pattern_wildcard_match() {
        let p = TestPattern::parse("test_*_end");
        assert!(p.matches(Path::new("f.sh"), "test_foo_end"));
        assert!(!p.matches(Path::new("f.sh"), "test_foo_end_extra"));
    }

    #[test]
    fn pattern_file_filter() {
        let p = TestPattern::parse("foo.sh/test_");
        assert!(p.matches(Path::new("/path/to/foo.sh"), "test_bar"));
        assert!(!p.matches(Path::new("/path/to/bar.sh"), "test_bar"));
    }

    #[test]
    fn pattern_file_subpath() {
        let p = TestPattern::parse("tests/foo.sh/test_");
        assert!(p.matches(Path::new("/repo/tests/foo.sh"), "test_bar"));
        assert!(!p.matches(Path::new("/repo/other/foo.sh"), "test_bar"));
    }

    fn write_script(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn functions_source_preserves_line_numbers() {
        // A helper, a blank line, then a test: every function line must land on
        // the same line number it has in the source, with the gaps blanked.
        let source = "helper() {\n  echo setup\n}\n\ntest_foo() {\n  echo a\n  false\n}\n";
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "lines.test", source);

        let tf = parse_test_file(&path).unwrap();
        let gen_lines: Vec<&str> = tf.script.lines().collect();
        let src_lines: Vec<&str> = source.lines().collect();

        // Function lines match the source exactly; the separator line is blanked.
        for idx in [0, 1, 2, 4, 5, 6, 7] {
            assert_eq!(gen_lines[idx], src_lines[idx], "line {idx} differs");
        }
        assert_eq!(gen_lines[3], "", "top-level/gap line 3 should be blank");
    }

    #[test]
    fn functions_source_falls_back_when_the_source_is_unavailable() {
        // Without the original text there are no line numbers to preserve, so
        // every function is re-rendered from the AST. Line alignment is lost
        // (and with it the diagnostic snippet), but the script must still be
        // valid shell that defines and runs each function.
        let source = "helper() {\n  echo 42\n}\n\ntest_foo() {\n  test \"$(helper)\" = 42\n}\n";
        let functions = parse_functions(source).unwrap();

        let generated = build_functions_source(&functions, "");
        let tmp = TempDir::new().unwrap();
        let script = write_script(tmp.path(), "fallback.sh", &generated);

        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &format!(". '{}'\ntest_foo\n", script.display())])
            .status()
            .unwrap();
        assert!(status.success(), "generated script failed:\n{generated}");
    }

    #[test]
    fn parse_file_with_test_functions() {
        let tmp = TempDir::new().unwrap();
        let source = "test_hello() {\n  echo hello\n}\n\ntest_world() {\n  echo world\n}\n";
        let path = write_script(tmp.path(), "example.test", source);

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.tests, ["test_hello", "test_world"]);
        // Nothing but function definitions here, so the script the runner
        // sources is the file itself, line for line.
        assert_eq!(result.script, source);
    }

    #[test]
    fn parse_file_with_helpers_and_tests() {
        let tmp = TempDir::new().unwrap();
        let source = "setup() {\n  mkdir -p /tmp/test\n}\n\ntest_foo() {\n  setup\n  echo foo\n}\n";
        let path = write_script(tmp.path(), "example.test", source);

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.tests, ["test_foo"]);
        // The helper is not a test but is still defined for the tests to call.
        assert_eq!(result.script, source);
    }

    #[test]
    fn parse_file_with_no_functions() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "plain.sh", "echo hello\necho world\n");

        let result = parse_test_file(&path).unwrap();
        assert!(result.tests.is_empty());
        // Top-level code is dropped, so sourcing the script runs nothing.
        assert!(result.script.is_empty());
    }

    #[test]
    fn parse_file_with_no_test_functions() {
        let tmp = TempDir::new().unwrap();
        let source = "helper_one() {\n  echo one\n}\n\nhelper_two() {\n  echo two\n}\n";
        let path = write_script(tmp.path(), "helpers.sh", source);

        let result = parse_test_file(&path).unwrap();
        assert!(result.tests.is_empty());
        assert_eq!(result.script, source);
    }

    #[test]
    fn parse_file_preserves_path() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "my.test", "test_a() {\n  true\n}\n");

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.path, path);
    }

    #[test]
    fn parse_nonexistent_file_errors() {
        let result = parse_test_file(Path::new("/nonexistent/file.test"));
        assert!(result.is_err());
    }

    #[test]
    fn test_prefix_is_strict() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(
            tmp.path(),
            "example.test",
            "testing() {\n  echo nope\n}\n\ntest_real() {\n  true\n}\n\nmy_test() {\n  echo no\n}\n",
        );

        let result = parse_test_file(&path).unwrap();
        // "testing" starts with "test" so it is a test; "my_test" does not start with "test"
        let names: Vec<&str> = result.tests.iter().map(String::as_str).collect();
        assert!(names.contains(&"testing"));
        assert!(names.contains(&"test_real"));
        assert!(!names.contains(&"my_test"));
    }

    #[test]
    fn extract_functions_from_nested_blocks() {
        let tmp = TempDir::new().unwrap();
        // Function defined inside an if block
        let path = write_script(
            tmp.path(),
            "nested.sh",
            "if true; then\n  test_inside_if() {\n    true\n  }\nfi\n\nfor x in 1; do\n  test_inside_for() {\n    true\n  }\ndone\n",
        );

        let result = parse_test_file(&path).unwrap();
        let names: Vec<&str> = result.tests.iter().map(String::as_str).collect();
        assert!(names.contains(&"test_inside_if"));
        assert!(names.contains(&"test_inside_for"));
    }

    #[test]
    fn duplicate_test_name_yields_single_test() {
        let tmp = TempDir::new().unwrap();
        // The same test name defined twice: the shell shadows the first, so
        // only the last definition can ever run. Exactly one test is emitted,
        // but both definitions stay in the sourced script, which is therefore
        // unchanged.
        let source = "test_dup() {\n  echo first\n}\n\ntest_dup() {\n  echo second\n}\n";
        let path = write_script(tmp.path(), "dup.test", source);

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.tests, ["test_dup"]);
        assert_eq!(result.script, source);
    }

    #[test]
    fn parse_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "empty.sh", "");

        let result = parse_test_file(&path).unwrap();
        assert!(result.tests.is_empty());
        assert!(result.script.is_empty());
    }
}

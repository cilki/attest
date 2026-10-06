use std::collections::HashSet;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::Context;
use brush_parser::ast::{Command, CompoundCommand, CompoundList, FunctionDefinition, Program};
use brush_parser::{Parser, ParserOptions};
use tracing::warn;

/// File containing tests.
pub struct TestFile {
    pub tests: Vec<TestCase>,
    pub functions: Vec<FunctionDefinition>,
}

/// Test function within a `TestFile`.
pub struct TestCase {
    pub file: PathBuf,
    pub name: String,
}

/// A pattern for selecting tests, parsed from `[<file>/]<name-pattern>`.
///
/// - `file`: if present, `test.file` must end with this path
/// - `name`: if present, matches the test function name; `*` is a wildcard;
///   a pattern without `*` matches any name that starts with the pattern
pub struct TestPattern {
    pub file: Option<PathBuf>,
    pub name: Option<String>,
}

impl TestPattern {
    pub fn parse(s: &str) -> Self {
        let is_path = s.starts_with("./");
        let s = s.strip_prefix("./").unwrap_or(s);
        // A `/` splits file from name; without one, a `./`-prefixed argument is
        // a bare file and anything else a bare test name. Either side may come
        // out empty (`foo.sh/`, `/test_x`), which means "unconstrained".
        let (file_part, name_part) = match s.rsplit_once('/') {
            Some(parts) => parts,
            None if is_path => (s, ""),
            None => ("", s),
        };
        Self {
            file: (!file_part.is_empty()).then(|| PathBuf::from(file_part)),
            name: (!name_part.is_empty()).then(|| name_part.to_string()),
        }
    }

    pub fn matches(&self, test: &TestCase) -> bool {
        if let Some(ref file_pat) = self.file
            && !test.file.ends_with(file_pat)
        {
            return false;
        }
        if let Some(ref name_pat) = self.name
            && !wildcard_match(name_pat, &test.name)
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
        tests.push(TestCase {
            file: path.to_path_buf(),
            name: f.fname.value.clone(),
        });
    }

    Ok(TestFile { tests, functions })
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
            Command::Simple(_) | Command::ExtendedTest(..) => {}
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
        CompoundCommand::Arithmetic(_) | CompoundCommand::Coprocess(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_case(file: &str, name: &str) -> TestCase {
        TestCase {
            file: PathBuf::from(file),
            name: name.to_string(),
        }
    }

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

        let p = TestPattern::parse("./foo.sh/test_bar");
        assert_eq!(p.file.as_deref(), Some(Path::new("foo.sh")));
        assert_eq!(p.name.as_deref(), Some("test_bar"));
    }

    #[test]
    fn pattern_prefix_match() {
        let p = TestPattern::parse("test_foo");
        assert!(p.matches(&make_case("/any/file.sh", "test_foo")));
        assert!(p.matches(&make_case("/any/file.sh", "test_foo_bar")));
        assert!(!p.matches(&make_case("/any/file.sh", "test_baz")));
    }

    #[test]
    fn pattern_wildcard_match() {
        let p = TestPattern::parse("test_*_end");
        assert!(p.matches(&make_case("f.sh", "test_foo_end")));
        assert!(!p.matches(&make_case("f.sh", "test_foo_end_extra")));
    }

    #[test]
    fn pattern_file_filter() {
        let p = TestPattern::parse("foo.sh/test_");
        assert!(p.matches(&make_case("/path/to/foo.sh", "test_bar")));
        assert!(!p.matches(&make_case("/path/to/bar.sh", "test_bar")));
    }

    #[test]
    fn pattern_file_subpath() {
        let p = TestPattern::parse("tests/foo.sh/test_");
        assert!(p.matches(&make_case("/repo/tests/foo.sh", "test_bar")));
        assert!(!p.matches(&make_case("/repo/other/foo.sh", "test_bar")));
    }

    fn write_script(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn parse_file_with_test_functions() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(
            tmp.path(),
            "example.test",
            "test_hello() {\n  echo hello\n}\n\ntest_world() {\n  echo world\n}\n",
        );

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.tests.len(), 2);
        assert_eq!(result.tests[0].name, "test_hello");
        assert_eq!(result.tests[1].name, "test_world");
        assert_eq!(result.functions.len(), 2);
    }

    #[test]
    fn parse_file_with_helpers_and_tests() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(
            tmp.path(),
            "example.test",
            "setup() {\n  mkdir -p /tmp/test\n}\n\ntest_foo() {\n  setup\n  echo foo\n}\n",
        );

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.tests.len(), 1);
        assert_eq!(result.tests[0].name, "test_foo");
        assert_eq!(result.functions.len(), 2);
    }

    #[test]
    fn parse_file_with_no_functions() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "plain.sh", "echo hello\necho world\n");

        let result = parse_test_file(&path).unwrap();
        assert!(result.tests.is_empty());
        assert!(result.functions.is_empty());
    }

    #[test]
    fn parse_file_with_no_test_functions() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(
            tmp.path(),
            "helpers.sh",
            "helper_one() {\n  echo one\n}\n\nhelper_two() {\n  echo two\n}\n",
        );

        let result = parse_test_file(&path).unwrap();
        assert!(result.tests.is_empty());
        assert_eq!(result.functions.len(), 2);
    }

    #[test]
    fn parse_file_preserves_path() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "my.test", "test_a() {\n  true\n}\n");

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.tests[0].file, path);
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
        let names: Vec<&str> = result.tests.iter().map(|t| t.name.as_str()).collect();
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
        let names: Vec<&str> = result.tests.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"test_inside_if"));
        assert!(names.contains(&"test_inside_for"));
    }

    #[test]
    fn duplicate_test_name_yields_single_test() {
        let tmp = TempDir::new().unwrap();
        // The same test name defined twice: the shell shadows the first, so
        // only the last definition can ever run. Exactly one test is emitted,
        // but both definitions remain in `functions` so the sourced script is
        // unchanged.
        let path = write_script(
            tmp.path(),
            "dup.test",
            "test_dup() {\n  echo first\n}\n\ntest_dup() {\n  echo second\n}\n",
        );

        let result = parse_test_file(&path).unwrap();
        assert_eq!(result.tests.len(), 1);
        assert_eq!(result.tests[0].name, "test_dup");
        assert_eq!(result.functions.len(), 2);
    }

    #[test]
    fn parse_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "empty.sh", "");

        let result = parse_test_file(&path).unwrap();
        assert!(result.tests.is_empty());
        assert!(result.functions.is_empty());
    }
}

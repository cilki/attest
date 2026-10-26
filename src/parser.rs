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
    /// The shell source that defines this file's functions, which the runner
    /// carries into the script it runs a test with (see
    /// [`build_functions_source`]). It is the same text for every test in the
    /// file, so it is rendered once here instead of per test: rendering it
    /// walks each function's AST to find its source span, which cost more than
    /// reading the file it is built from — and a file holding 25 tests paid it
    /// 25 times over, once more for every `--repeat`. Keeping the text rather
    /// than the AST it came from is also what lets the parse be thrown away:
    /// nothing downstream needs the nodes.
    ///
    /// Empty when [`TestFile::tests`] is: the definitions exist to be run by a
    /// run of one of this file's tests, so a file with no test in it has
    /// nothing to run them (see [`parse_test_file`]).
    pub script: String,
    /// The shell the runner execs to define [`TestFile::script`] and call one
    /// of [`TestFile::tests`], unless `--shebang` overrides it (see
    /// [`crate::discovery::script_shell`]).
    ///
    /// Kept here for the same reason the script is: it depends only on the
    /// file, and deriving it needs the file's first line — which the runner
    /// used to reopen and re-read on every spawn, and so once more for every
    /// `--repeat`. The text this parse read is already the first line.
    pub shell: String,
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

/// The file half of a selector: the path a test was discovered at, with a
/// leading `./` dropped.
///
/// The path, not just the base name. Two things need it that way: the file half
/// of a selector is matched as a path *suffix*, so a bare `x.test/test_foo`
/// names every `x.test` in the tree while `a/x.test/test_foo` names exactly
/// one; and a positional target has to have a real path on its left
/// (`split_path_arg` only splits where the left side is an existing file), so
/// only a selector carrying the directories it was found under can be pasted
/// back as `attest <selector>` from the directory it was printed in.
///
/// Dropping the `./` is not cosmetic. Behind one, [`TestPattern::parse`] takes
/// the *whole* argument as a path, so `./a/x.test/test_foo` as a `--filter`
/// asks for a file named `a/x.test/test_foo` and matches nothing. A walk
/// rooted at `.` yields `./a/x.test`, so the marker is exactly what discovery
/// hands over.
pub(crate) fn selector_path(file: &Path) -> String {
    let path = file.to_string_lossy();
    path.strip_prefix("./").unwrap_or(&path).to_string()
}

/// Format a `(file, test name)` pair as the `<file>/<name>` selector that
/// `--filter` and the positional target accept.
///
/// The one spelling of the join, as `split_file_and_name` is the one spelling
/// of the split: `list` prints these, completion offers them, and
/// [`TestPattern::parse`] reads them back, so the three have to agree about
/// what a selector looks like — including the `./` rule above, which an
/// open-coded `format!("{file}/{name}")` quietly got wrong.
pub(crate) fn selector(file: &Path, name: &str) -> String {
    format!("{}/{name}", selector_path(file))
}

/// What a selection rules out about a file before the file is parsed: where it
/// sits, and what its text spells.
///
/// Parsing is nearly all of what a run does before it spawns anything —
/// `brush_parser` costs a few microseconds a line and the walk hands it every
/// script in the tree — and a `--filter` normally excludes almost all of it.
/// Both halves of a [`TestPattern`] narrow the candidates without a parse: the
/// file half is a path suffix, and the name half says what the name of the
/// definition in the text has to start with, which is the same question the
/// bare `test…` scan already answers.
///
/// It may only ever err towards the parse, exactly as that scan may: a file it
/// admits but that holds nothing selectable costs a parse that would have
/// happened anyway, while one it wrongly rejects loses a test. The price of a
/// rejection is that nothing is reported about the file either — an
/// unparseable script the filter excludes is no longer named on stderr, and
/// neither is a test such a file defines twice. That is the bargain the
/// `test…` scan already makes: a file no selected test can come out of is a
/// file the run has nothing to say about.
pub struct Prescan {
    /// The pattern's file half: the file's path must end with it (whole
    /// components, no wildcards — see [`TestPattern`]).
    file: Option<PathBuf>,
    /// The literal prefix a definition's name has to start with for this
    /// selection to be able to pick it.
    ///
    /// `None` when the filter's name half and the `test…` rule exclude one
    /// another (`--filter overlay`): no name satisfies both, so no file in the
    /// tree is worth parsing.
    name_prefix: Option<String>,
}

impl Default for Prescan {
    /// Unconstrained — every test in every file is selectable, which is what
    /// `list` without a filter and shell completion ask for. The `test…` rule
    /// still applies: it is a property of what a test *is*, not of the
    /// selection.
    fn default() -> Self {
        Self {
            file: None,
            name_prefix: Some(TEST_PREFIX.to_string()),
        }
    }
}

impl Prescan {
    /// What `pattern` rules out.
    pub fn of(pattern: &TestPattern) -> Self {
        // [`wildcard_match`] requires a matching name to start with
        // everything in front of the pattern's first `*`, and a pattern with
        // no `*` at all matches as a prefix, so the whole of it is literal.
        let literal = pattern
            .name
            .as_deref()
            .map_or("", |n| n.split('*').next().unwrap_or(""));
        // That prefix and `test` are both prefixes of the same name, so one
        // has to contain the other and the longer of the two is the whole
        // constraint. If neither extends the other, no name can carry both.
        let name_prefix = if literal.starts_with(TEST_PREFIX) {
            Some(literal.to_string())
        } else if TEST_PREFIX.starts_with(literal) {
            Some(TEST_PREFIX.to_string())
        } else {
            None
        };
        Self {
            file: pattern.file.clone(),
            name_prefix,
        }
    }

    /// Could a test this selection picks be defined in the file at `path`?
    ///
    /// Settled from the path alone, so a file it rejects is never even read.
    /// The same `Path::ends_with` suffix [`TestPattern::matches`] applies, so
    /// the two cannot disagree about which files a filter names.
    pub fn admits(&self, path: &Path) -> bool {
        self.file
            .as_ref()
            .is_none_or(|suffix| path.ends_with(suffix))
    }

    /// Could `source` define a function this selection picks?
    ///
    /// A full parse is the only thing that *answers* that, but it costs
    /// roughly a microsecond per line of script, and a directory walk would
    /// otherwise parse every shell script it turns up — the vast majority of
    /// which, in a repository of any size, define no test at all, and under a
    /// filter none that is wanted. So the text is scanned first for the one
    /// shape a definition can have: the literal prefix at the head of a word,
    /// the rest of a name, then (across any blanks or newlines) the `(` of
    /// `test_foo()` or the `{` of `function test_foo {`.
    ///
    /// The scan only ever has to agree with the parser about what *cannot* be
    /// there. Hence the deliberate crudeness — the name bytes are "not a
    /// blank, not a metacharacter", the word head is "not preceded by a name
    /// byte", and anything may sit between the name and its brace. A name the
    /// shell only spells `test…` after quote removal (`te'st'_foo`) is not a
    /// false negative: brush keeps the quotes in the name, so such a function
    /// is not a test today either.
    fn may_define(&self, source: &str) -> bool {
        let Some(prefix) = self.name_prefix.as_deref() else {
            return false;
        };
        let b = source.as_bytes();
        for (i, _) in source.match_indices(prefix) {
            // The tail of a longer word, as in `latest_release() {`.
            if i > 0 && is_name_byte(b[i - 1]) {
                continue;
            }
            let mut j = i + prefix.len();
            while j < b.len() && is_name_byte(b[j]) {
                j += 1;
            }
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches!(b.get(j), Some(b'(' | b'{')) {
                return true;
            }
        }
        false
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

/// What a function's name has to start with for it to be a test. One
/// spelling, shared by the scan that decides whether a file is worth parsing
/// and by the walk over what the parse found.
const TEST_PREFIX: &str = "test";

/// Is `b` a byte a function name can be spelled with?
///
/// Everything that is not a blank and not one of the shell's own
/// metacharacters: a shell takes almost anything else as a function name
/// (`test-dash`, `test.dot`, `tëst` — and brush parses all three), so the set
/// is defined by what it excludes rather than by what it allows.
fn is_name_byte(b: u8) -> bool {
    !b.is_ascii_whitespace()
        && !matches!(
            b,
            b'(' | b')'
                | b'{'
                | b'}'
                | b'|'
                | b'&'
                | b';'
                | b'<'
                | b'>'
                | b'#'
                | b'\''
                | b'"'
                | b'`'
                | b'$'
                | b'\\'
        )
}

/// Parse `path` for the tests `scan` can select out of it.
///
/// `scan` decides only whether the parse happens at all, never what it finds:
/// a file it admits is parsed whole and reports every test in it, since the
/// pattern still has the final say (see [`Prescan`]).
pub fn parse_test_file(path: &Path, scan: &Prescan) -> anyhow::Result<TestFile> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Nothing in this file can be selected, so there is nothing a parse of it
    // could tell a run: the tests are the names it does not hold, and the
    // definitions are only ever run to call one of them.
    if !scan.may_define(&contents) {
        return Ok(TestFile {
            path: path.to_path_buf(),
            tests: Vec::new(),
            script: String::new(),
            shell: crate::discovery::script_shell(&contents),
        });
    }

    let functions = parse_functions(&contents)
        .map_err(|e| anyhow::anyhow!("parse error in {}: {e}", path.display()))?;

    // A shell function defined more than once in the same file is shadowed:
    // when the runner defines them all and calls the name, only the last
    // definition runs. Emit a single test per name (matching what the shell
    // actually executes) rather than one phantom test per definition — every
    // one of which would run the same, last body. `functions` still carries
    // all definitions so the rendered script is unchanged.
    let mut tests = Vec::new();
    let mut seen = HashSet::new();
    for f in &functions {
        if !f.fname.value.starts_with(TEST_PREFIX) {
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
        // Both built here, from the text this parse came from, so the script,
        // the spans it is cut with and the shell that runs it can never be
        // out of step: the file is read once and all three come from that one
        // read.
        script: build_functions_source(&functions, &contents),
        shell: crate::discovery::script_shell(&contents),
    })
}

/// Build the definitions the runner puts at the head of a test's script.
///
/// Each function is emitted verbatim at the same line number it occupies in the
/// original source, with the gaps between them blanked out. Because the shell's
/// `$LINENO` (used by our xtrace `PS4`) reports the line within the script it
/// is running, and the runner puts these definitions at the head of that
/// script, keeping functions at their original offsets makes xtrace line
/// numbers match the source exactly — so a failing command's trace points
/// straight at its source line, with no reconstruction or line-mapping
/// heuristics needed on the diagnostics side.
///
/// Only function definitions are emitted (top-level code is dropped), so
/// running the script never runs a test file's top-level statements.
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

        let tf = parse_test_file(&path, &Prescan::default()).unwrap();
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

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
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

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
        assert_eq!(result.tests, ["test_foo"]);
        // The helper is not a test but is still defined for the tests to call.
        assert_eq!(result.script, source);
    }

    #[test]
    fn parse_file_with_no_functions() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "plain.sh", "echo hello\necho world\n");

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
        assert!(result.tests.is_empty());
        // Top-level code is dropped, so sourcing the script runs nothing.
        assert!(result.script.is_empty());
    }

    #[test]
    fn parse_file_with_no_test_functions() {
        let tmp = TempDir::new().unwrap();
        let source = "helper_one() {\n  echo one\n}\n\nhelper_two() {\n  echo two\n}\n";
        let path = write_script(tmp.path(), "helpers.sh", source);

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
        assert!(result.tests.is_empty());
        // Nothing in the file can be selected, so nothing will ever source its
        // functions: the script is not built (and the file is not even parsed).
        assert!(result.script.is_empty());
    }

    /// Every spelling of a function definition the parser recognizes has to
    /// survive the scan that decides whether the parser is worth running: a
    /// false negative there is a test that silently stops existing.
    #[test]
    fn the_scan_keeps_every_test_the_parser_would_find() {
        let spellings = [
            ("test_plain() {\n  true\n}\n", "test_plain"),
            ("test_spaced ( ) {\n  true\n}\n", "test_spaced"),
            ("function test_keyword {\n  true\n}\n", "test_keyword"),
            ("function test_both() {\n  true\n}\n", "test_both"),
            // The brace on the line after the name.
            ("function test_nextline\n{\n  true\n}\n", "test_nextline"),
            // A subshell body instead of a brace group.
            ("test_subshell() (\n  true\n)\n", "test_subshell"),
            // Shells take far more than `[A-Za-z0-9_]` in a function name.
            ("test-dash() {\n  true\n}\n", "test-dash"),
            ("test.dot() {\n  true\n}\n", "test.dot"),
            // Indented, and after another command on the same line.
            ("  test_indented() {\n  true\n}\n", "test_indented"),
            ("true; test_after() {\n  true\n}\n", "test_after"),
            // Nested inside a compound command.
            (
                "if true; then\n  test_nested() {\n    true\n  }\nfi\n",
                "test_nested",
            ),
        ];

        let tmp = TempDir::new().unwrap();
        for (source, name) in spellings {
            assert!(
                Prescan::default().may_define(source),
                "scan rejected {source:?}"
            );
            let path = write_script(tmp.path(), "spelling.test", source);
            let parsed = parse_test_file(&path, &Prescan::default()).unwrap();
            assert_eq!(parsed.tests, [name], "parse of {source:?}");
        }
    }

    /// The point of the scan: an ordinary script pays for a read, not for a
    /// shell parse. `test` as a command, and names that merely end in it,
    /// define nothing a run could select.
    #[test]
    fn the_scan_skips_what_cannot_define_a_test() {
        for source in [
            "",
            "#!/bin/sh\necho hello\n",
            "if test -f /etc/passwd; then echo yes; fi\n",
            "x=$(test -d /tmp && echo yes)\n",
            "latest_release() {\n  true\n}\n",
            "my_test() {\n  true\n}\n",
            "contest=1\n",
            "# the tests live in t/\n",
            // Quoting that only spells `test…` after the shell removes it is
            // not a test today either: brush keeps the quotes in the name, so
            // the scan and the parser agree about it.
            "te'st'_quoted() {\n  true\n}\n",
        ] {
            assert!(
                !Prescan::default().may_define(source),
                "scan accepted {source:?}"
            );
            // And the parser agrees: skipping the parse loses nothing, which
            // is the only thing the scan is allowed to be right about.
            let functions = parse_functions(source).expect(source);
            assert!(
                !functions
                    .iter()
                    .any(|f| f.fname.value.starts_with(TEST_PREFIX)),
                "parser found a test in {source:?}"
            );
        }
    }

    /// A filter's file half decides which files are worth reading at all, and
    /// it has to decide it the same way [`TestPattern::matches`] does: a whole
    /// path suffix, so a bare base name names every file of that name in the
    /// tree and a longer one narrows it.
    #[test]
    fn the_prescan_admits_exactly_the_files_the_filter_names() {
        let scan = Prescan::of(&TestPattern::parse("tests/foo.sh/test_x"));
        assert!(scan.admits(Path::new("/repo/tests/foo.sh")));
        // Same name, wrong directory; and the right directory is matched by
        // whole components, not by a string suffix.
        assert!(!scan.admits(Path::new("/repo/other/foo.sh")));
        assert!(!scan.admits(Path::new("/repo/mytests/foo.sh")));
        assert!(!scan.admits(Path::new("/repo/tests/bar.sh")));

        // No file half, so the path constrains nothing — every file is a
        // candidate, which is also what no filter at all means.
        for scan in [
            Prescan::of(&TestPattern::parse("test_x")),
            Prescan::default(),
        ] {
            assert!(scan.admits(Path::new("/repo/tests/foo.sh")));
            assert!(scan.admits(Path::new("anything")));
        }
    }

    /// A filter's name half is a prefix of whatever it matches, so it says
    /// what a definition in the file has to be called — and a file that
    /// spells no such name holds nothing the filter can select.
    #[test]
    fn the_prescan_reads_the_filter_as_a_name_prefix() {
        let scan = Prescan::of(&TestPattern::parse("test_alpha"));
        assert!(scan.may_define("test_alpha() {\n  true\n}\n"));
        // A longer name still starts with the pattern, since a pattern
        // without a `*` matches as a prefix.
        assert!(scan.may_define("test_alpha_and_more() {\n  true\n}\n"));
        // A test, but not one this filter can ever pick.
        assert!(!scan.may_define("test_beta() {\n  true\n}\n"));

        // Everything in front of the first `*` is literal; the rest cannot
        // narrow the prefix.
        let scan = Prescan::of(&TestPattern::parse("test_al*ha"));
        assert!(scan.may_define("test_alpha() {\n  true\n}\n"));
        assert!(!scan.may_define("test_beta() {\n  true\n}\n"));

        // A name half shorter than `test` leaves the `test…` rule in charge:
        // it is the stronger of the two constraints.
        let scan = Prescan::of(&TestPattern::parse("te"));
        assert!(scan.may_define("test_alpha() {\n  true\n}\n"));
        assert!(!scan.may_define("telescope() {\n  true\n}\n"));

        // A file-only filter constrains no name at all.
        let scan = Prescan::of(&TestPattern::parse("foo.sh/"));
        assert!(scan.may_define("test_anything() {\n  true\n}\n"));
        assert!(!scan.may_define("helper() {\n  true\n}\n"));
    }

    /// A name half that is not `test…` and is not a prefix of it selects
    /// nothing, whatever any file holds — so no file is worth parsing.
    #[test]
    fn the_prescan_rejects_everything_for_an_unsatisfiable_filter() {
        let scan = Prescan::of(&TestPattern::parse("overlay"));
        assert!(!scan.may_define("test_overlay() {\n  true\n}\n"));
        // And the pattern itself agrees that nothing was lost.
        let pattern = TestPattern::parse("overlay");
        assert!(!pattern.matches(Path::new("a.test"), "test_overlay"));
    }

    /// The scan is allowed to be wrong in the direction that only costs a
    /// parse: `test` followed by something brace-shaped is handed over, and
    /// the parser has the final say about whether it defines anything.
    #[test]
    fn a_false_positive_only_costs_the_parse() {
        let source = "test_like_this_but_not_a_definition=(a b)\n";
        assert!(Prescan::default().may_define(source));

        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "array.sh", source);
        assert!(
            parse_test_file(&path, &Prescan::default())
                .unwrap()
                .tests
                .is_empty()
        );
    }

    #[test]
    fn parse_file_preserves_path() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "my.test", "test_a() {\n  true\n}\n");

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
        assert_eq!(result.path, path);
    }

    #[test]
    fn parse_nonexistent_file_errors() {
        let result = parse_test_file(Path::new("/nonexistent/file.test"), &Prescan::default());
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

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
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

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
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

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
        assert_eq!(result.tests, ["test_dup"]);
        assert_eq!(result.script, source);
    }

    #[test]
    fn parse_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_script(tmp.path(), "empty.sh", "");

        let result = parse_test_file(&path, &Prescan::default()).unwrap();
        assert!(result.tests.is_empty());
        assert!(result.script.is_empty());
    }
}

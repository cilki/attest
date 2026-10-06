#[cfg(feature = "cgroup")]
mod cgroup;
mod diagnostics;
mod discovery;
mod output;
mod overlay;
mod parser;
mod runner;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use std::path::{Path, PathBuf};

fn parse_fuzz(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{}' is not a valid number", s))?;
    if v > 0.0 && v < 1.0 {
        Ok(v)
    } else {
        Err(format!(
            "fuzz value must be strictly between 0 and 1, got {}",
            v
        ))
    }
}

fn parse_timeout(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{}' is not a valid number", s))?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(format!(
            "timeout must be a positive number of seconds, got {}",
            v
        ))
    }
}

#[derive(Parser)]
#[command(
    version,
    name = "attest",
    about = "Dead simple test framework for the age of AI"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Test target: a file, directory, or `<file>/<test>` pattern
    #[arg(add = ArgValueCompleter::new(complete_tests))]
    path: Option<String>,

    /// Maximum number of tests to run in parallel
    #[arg(long, default_value_t = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    parallel: usize,

    /// Filter tests by pattern: `[<file>/]<name>` where `<name>` supports `*` wildcards and
    /// plain names match as prefixes (e.g. `foo.sh/test_net*`)
    #[arg(long, add = ArgValueCompleter::new(complete_tests))]
    filter: Option<String>,

    /// Copy each test's logs, filesystem delta and working directory into DIR
    /// for debugging. The context directories themselves are always temporary
    /// and are still removed on exit.
    #[arg(long, value_name = "DIR")]
    save_context: Option<PathBuf>,

    /// Stop after first test failure
    #[arg(long)]
    bail: bool,

    /// Increase output verbosity (-v: per-test PASS/FAIL lines, -vv: live
    /// xtrace streaming, one test at a time)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Override a command in the test context bin/ dir. Accepts a path
    /// (`/usr/bin/example` or `./bin/example`) or a mapping
    /// (`example=/usr/bin/overridden`). Can be specified multiple times.
    #[arg(long)]
    r#override: Vec<runner::OverrideSpec>,

    /// Add a directory of executables to each test's PATH (lower precedence than
    /// --override, higher than the inherited PATH). Relative paths are resolved
    /// against the current directory at invocation time. Can be specified multiple times.
    #[arg(long)]
    bin_dir: Vec<PathBuf>,

    /// Trace a command with strace. CMD is a command name (`curl`) or a path to
    /// one (`/usr/bin/curl`); either way tests calling it by its base name get
    /// the traced version. The log is written to the test's context dir, so
    /// pair this with --save-context to keep it. Can be specified multiple
    /// times.
    #[arg(long, value_name = "CMD")]
    strace: Vec<String>,

    /// Kill a test and mark it as timed out after this many seconds (wall-clock time)
    #[arg(long, value_parser = parse_timeout)]
    timeout: Option<f64>,

    /// Print results as JSONL (one JSON object per test) instead of terminal output
    #[arg(long)]
    json: bool,

    /// Randomly pause and resume individual descendant processes of each test to
    /// introduce timing non-determinism. Optionally accepts an aggressiveness value
    /// in (0,1) where higher values pause processes more frequently (default: 0.5)
    #[arg(long, num_args = 0..=1, default_missing_value = "0.5", value_parser = parse_fuzz)]
    fuzz: Option<f64>,

    /// Run each test this many times
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    repeat: u64,

    /// Override the shell used to run tests, ignoring each script's own shebang
    /// (e.g. `/bin/bash`, `/usr/bin/zsh`)
    #[arg(long)]
    shebang: Option<String>,

    /// Disable overlayfs isolation; run each test directly in the working directory
    #[arg(long)]
    no_overlay: bool,

    /// Disable cgroup resource tracking
    #[cfg(feature = "cgroup")]
    #[arg(long)]
    no_cgroups: bool,

    /// Enable debug logging
    #[arg(short = 'd', long)]
    debug: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// List test functions without running them
    List {
        /// Test target: a file, directory, or `<file>/<test>` pattern
        #[arg(add = ArgValueCompleter::new(complete_tests))]
        path: String,

        /// Filter tests by pattern: `[<file>/]<name>` where `<name>` supports `*` wildcards and
        /// plain names match as prefixes (e.g. `foo.sh/test_net*`)
        #[arg(long, add = ArgValueCompleter::new(complete_tests))]
        filter: Option<String>,
    },
    /// Print AI agent skill for writing .test files
    Skill,
}

/// Unique display name for a test. A name that appears in more than one
/// selected file is qualified with its file name (`a.test:test_foo`) so
/// results are tellable-apart and per-test context directories, which are
/// keyed by display name, never collide; a numeric suffix breaks any
/// remaining tie (same name defined twice in one file).
fn display_base(
    name: &str,
    file: &Path,
    duplicated: bool,
    taken: &mut std::collections::HashSet<String>,
) -> String {
    let base = if duplicated {
        let file_name = file
            .file_name()
            .map(|f| f.to_string_lossy())
            .unwrap_or_default();
        format!("{file_name}:{name}")
    } else {
        name.to_string()
    };
    if taken.insert(base.clone()) {
        return base;
    }
    let mut k = 2;
    loop {
        let candidate = format!("{base}:{k}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        k += 1;
    }
}

/// Split a positional argument into a file path and an optional test filter.
///
/// If the argument contains a `/` where the left side is an existing file,
/// it is treated as `file/filter`. Otherwise the whole string is a path.
fn split_path_arg(arg: &str) -> (PathBuf, Option<String>) {
    // Try splitting from the right on `/` — the left side must be an existing file
    if let Some(slash) = arg.rfind('/') {
        let file_part = &arg[..slash];
        let name_part = &arg[slash + 1..];
        let path = Path::new(file_part);
        if path.is_file() {
            let filter = if name_part.is_empty() {
                None
            } else {
                Some(format!("{file_part}/{name_part}"))
            };
            return (path.to_path_buf(), filter);
        }
    }
    (PathBuf::from(arg), None)
}

/// Parse every discovered file, returning the files that parsed together with
/// their parse results (the two vectors stay index-aligned).
///
/// When the target was a single file the user named, a parse error is the
/// answer to what they asked for and stays fatal. A directory target is
/// different: the walk picks up *every* shell script in the tree, most of
/// which were never meant to hold tests, so one script the parser cannot
/// handle — zsh-only syntax, a generated or templated file, a genuine syntax
/// error in something unrelated — must not abort a run it contributes no tests
/// to. Those are reported on stderr and skipped.
fn parse_discovered(
    files: Vec<PathBuf>,
    explicit_file: bool,
) -> anyhow::Result<(Vec<PathBuf>, Vec<parser::TestFile>)> {
    let mut kept = Vec::with_capacity(files.len());
    let mut parsed = Vec::with_capacity(files.len());
    for file in files {
        match parser::parse_test_file(&file) {
            Ok(test_file) => {
                kept.push(file);
                parsed.push(test_file);
            }
            Err(e) if explicit_file => return Err(e),
            // Printed directly rather than logged: dropping a file changes
            // what the run covers, so it must show up whatever the log level
            // happens to be.
            Err(e) => eprintln!("warning: {e}; skipping this file"),
        }
    }
    Ok((kept, parsed))
}

fn complete_tests(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let current = current.to_string_lossy();
    let mut candidates = Vec::new();

    // If input contains `/` and left side is a file, complete test names within it
    if let Some(slash) = current.rfind('/') {
        let file_part = &current[..slash];
        let name_prefix = &current[slash + 1..];
        let path = Path::new(file_part);

        if path.is_file() {
            if let Ok(test_file) = parser::parse_test_file(path) {
                for test in &test_file.tests {
                    if test.name.starts_with(name_prefix) {
                        candidates.push(CompletionCandidate::new(format!(
                            "{}/{}",
                            file_part, test.name
                        )));
                    }
                }
            }
            return candidates;
        }
    }

    // No file/test split — complete with discovered files and all file/test patterns
    let cwd = Path::new(".");
    if let Ok(files) = discovery::discover_test_files(cwd) {
        for file in &files {
            let rel = file.strip_prefix(cwd).unwrap_or(file).display().to_string();
            let rel = rel.strip_prefix("./").unwrap_or(&rel).to_string();

            if rel.starts_with(&*current) {
                candidates.push(CompletionCandidate::new(&rel));
            }

            if let Ok(test_file) = parser::parse_test_file(file) {
                for test in &test_file.tests {
                    let full = format!("{}/{}", rel, test.name);
                    if full.starts_with(&*current) {
                        candidates.push(CompletionCandidate::new(full));
                    }
                }
            }
        }
    }

    candidates
}

fn main() -> anyhow::Result<()> {
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    let env_filter = if cli.debug {
        tracing_subscriber::EnvFilter::new("attest=debug")
    } else {
        tracing_subscriber::EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    match cli.command {
        Some(Commands::Skill) => {
            print!("{}", include_str!("../SKILL.md"));
        }
        Some(Commands::List { path, filter }) => {
            let (path, inline_filter) = split_path_arg(&path);
            let filter = filter.or(inline_filter);
            let pattern = filter.as_deref().map(parser::TestPattern::parse);
            let files = discovery::discover_test_files(&path)?;
            let (_, parsed) = parse_discovered(files, path.is_file())?;
            let mut tests = Vec::new();
            for test_file in parsed {
                for test in test_file.tests {
                    if let Some(ref p) = pattern
                        && !p.matches(&test)
                    {
                        continue;
                    }
                    tests.push(test);
                }
            }
            output::print_test_list(&tests);
        }
        None => {
            let path_arg = cli.path.unwrap_or_else(|| {
                eprintln!("error: a path to a test file or directory is required");
                std::process::exit(1);
            });

            let (path, inline_filter) = split_path_arg(&path_arg);
            let filter = cli.filter.or(inline_filter);
            let pattern = filter.as_deref().map(parser::TestPattern::parse);
            let files = discovery::discover_test_files(&path)?;

            // Parse each file exactly once and keep the owning `TestFile`s
            // alive, so every test can borrow its file's extracted functions
            // rather than cloning the whole AST per test (and again per repeat).
            let (files, parsed) = parse_discovered(files, path.is_file())?;

            // Collect matching tests, tallying how often each name occurs so
            // duplicates across files can be given distinct display names. Each
            // selection records the index of the file it came from.
            let mut selected: Vec<(String, usize)> = Vec::new();
            let mut name_counts: std::collections::HashMap<String, usize> = Default::default();
            for (idx, test_file) in parsed.iter().enumerate() {
                for test in &test_file.tests {
                    if let Some(ref p) = pattern
                        && !p.matches(test)
                    {
                        continue;
                    }
                    *name_counts.entry(test.name.clone()).or_default() += 1;
                    selected.push((test.name.clone(), idx));
                }
            }
            if selected.is_empty() {
                match &filter {
                    Some(f) => anyhow::bail!("no tests match filter '{f}'"),
                    None => anyhow::bail!("no test functions found in {}", path.display()),
                }
            }

            // Display names key result output, per-test context dirs, and
            // --save-context dirs, so they must be unique per test.
            let mut taken = std::collections::HashSet::new();
            let mut all_tests: Vec<(String, String, usize)> = Vec::new();
            for (name, idx) in selected {
                let base = display_base(&name, &files[idx], name_counts[&name] > 1, &mut taken);
                for i in 1..=cli.repeat {
                    let display = if cli.repeat > 1 {
                        format!("{base}#{i}")
                    } else {
                        base.clone()
                    };
                    all_tests.push((display, name.clone(), idx));
                }
            }

            // Make --bin-dir entries absolute up front. They are embedded
            // literally into each test's PATH, which must stay valid after a
            // test cd's somewhere else — a relative entry would silently stop
            // resolving. Erroring here (rather than silently) surfaces a clear
            // message, consistent with how --override validates its source.
            let bin_dirs = cli
                .bin_dir
                .into_iter()
                .map(|dir| {
                    dir.canonicalize()
                        .map_err(|e| anyhow::anyhow!("--bin-dir: {} ({e})", dir.display()))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            // An explicit --shebang must name a shell that actually exists.
            // Otherwise tests would silently fall back to /bin/sh and report
            // results for a shell the user never asked for.
            if let Some(ref shell) = cli.shebang
                && !runner::shell_exists(shell)
            {
                anyhow::bail!("--shebang: shell not found: {shell}");
            }

            let config = runner::RunConfig {
                parallel: cli.parallel,
                bail: cli.bail,
                verbose: cli.verbose,
                json: cli.json,
                save_context: cli.save_context,
                override_cmds: cli.r#override,
                bin_dirs,
                strace: cli.strace,
                timeout: cli.timeout.map(std::time::Duration::from_secs_f64),
                fuzz: cli.fuzz,
                shebang: cli.shebang,
                no_overlay: cli.no_overlay,
                #[cfg(feature = "cgroup")]
                no_cgroups: cli.no_cgroups,
            };

            let test_refs: Vec<runner::TestSpec> = all_tests
                .iter()
                .map(|(display, fn_name, idx)| {
                    (
                        display.as_str(),
                        fn_name.as_str(),
                        parsed[*idx].functions.as_slice(),
                        files[*idx].as_path(),
                    )
                })
                .collect();

            let results = runner::run_all_tests(test_refs, &config)?;

            if results.iter().any(|r| !r.passed) {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const GOOD: &str = "#!/bin/sh\ntestGood() {\n\ttrue\n}\n";
    // Unterminated double quote: valid for no shell, and the kind of thing a
    // tree can contain without it having anything to do with the tests.
    const BROKEN: &str = "#!/bin/bash\nfoo() {\n\techo \"unterminated\n}\n";

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn parse_discovered_skips_unparseable_scripts_found_by_a_walk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let broken = script(tmp.path(), "broken.sh", BROKEN);
        let good = script(tmp.path(), "good.test", GOOD);

        let (kept, parsed) = parse_discovered(vec![broken, good.clone()], false)
            .expect("one bad script must not abort a directory run");

        // Kept files and their parse results stay index-aligned.
        assert_eq!(kept, vec![good]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].tests.iter().map(|t| &t.name).collect::<Vec<_>>(),
            vec!["testGood"]
        );
    }

    #[test]
    fn parse_discovered_is_fatal_for_an_explicitly_named_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let broken = script(tmp.path(), "broken.sh", BROKEN);

        let Err(err) = parse_discovered(vec![broken], true) else {
            panic!("a file the user named must not be skipped silently");
        };
        assert!(err.to_string().starts_with("parse error in "), "{err}");
    }

    #[test]
    fn display_base_unqualified_when_unique() {
        let mut taken = HashSet::new();
        assert_eq!(
            display_base("test_a", Path::new("x.test"), false, &mut taken),
            "test_a"
        );
    }

    #[test]
    fn display_base_qualifies_duplicates_with_file_name() {
        let mut taken = HashSet::new();
        assert_eq!(
            display_base("test_dup", Path::new("dir/a.test"), true, &mut taken),
            "a.test:test_dup"
        );
        assert_eq!(
            display_base("test_dup", Path::new("dir/b.test"), true, &mut taken),
            "b.test:test_dup"
        );
    }

    #[test]
    fn display_base_breaks_remaining_ties_numerically() {
        let mut taken = HashSet::new();
        assert_eq!(
            display_base("test_dup", Path::new("a/x.test"), true, &mut taken),
            "x.test:test_dup"
        );
        // Same file name in a different directory collides on the qualified
        // form; the numeric suffix disambiguates.
        assert_eq!(
            display_base("test_dup", Path::new("b/x.test"), true, &mut taken),
            "x.test:test_dup:2"
        );
        assert_eq!(
            display_base("test_dup", Path::new("c/x.test"), true, &mut taken),
            "x.test:test_dup:3"
        );
    }
}

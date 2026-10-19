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

use crate::output::errln;

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
    #[arg(add = ArgValueCompleter::new(complete_target))]
    path: Option<String>,

    /// Maximum number of tests to run in parallel
    #[arg(long, default_value_t = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    parallel: usize,

    /// Filter tests by pattern: `[<file>/]<name>` where `<name>` supports `*` wildcards and
    /// plain names match as prefixes (e.g. `foo.sh/test_net*`)
    #[arg(long, add = ArgValueCompleter::new(complete_filter))]
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
        #[arg(add = ArgValueCompleter::new(complete_target))]
        path: String,

        /// Filter tests by pattern: `[<file>/]<name>` where `<name>` supports `*` wildcards and
        /// plain names match as prefixes (e.g. `foo.sh/test_net*`)
        #[arg(long, add = ArgValueCompleter::new(complete_filter))]
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

/// Split a `<file>/<name>` argument on its last `/`, but only where the left
/// half is an existing file — otherwise the whole argument is a path.
///
/// The single home of that rule: a positional target, and the completion of
/// one, have to split identically or a completed selector names something
/// other than the test it was completed from.
fn split_file_and_name(arg: &str) -> Option<(&str, &str)> {
    let (file, name) = arg.rsplit_once('/')?;
    Path::new(file).is_file().then_some((file, name))
}

/// Split a positional argument into a file path and an optional test filter.
fn split_path_arg(arg: &str) -> (PathBuf, Option<String>) {
    let Some((file, name)) = split_file_and_name(arg) else {
        return (PathBuf::from(arg), None);
    };
    // A trailing `/` names the file and nothing within it, so there is no
    // filter to derive — the file half alone is already the whole selection.
    let filter = (!name.is_empty()).then(|| {
        // The `./` of a `./a/x.test/test_foo` target is noise in the selector
        // and, as a prefix, tells `TestPattern::parse` that the whole argument
        // is a path — which would swallow the name half we just established is
        // one. The file half matches as a suffix, so dropping it changes
        // nothing else.
        let file = file.strip_prefix("./").unwrap_or(file);
        format!("{file}/{name}")
    });
    (PathBuf::from(file), filter)
}

/// Parse every discovered file, returning the files that parsed. Each
/// [`parser::TestFile`] carries the path it came from, so nothing else has to
/// be kept alongside.
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
    cores: Cores,
) -> anyhow::Result<Vec<parser::TestFile>> {
    let mut parsed = Vec::with_capacity(files.len());
    // Reported here, on this thread and in discovery order, however the parse
    // was spread: which file was skipped is part of what the run covers, so
    // the warnings have to read the same way from one run to the next.
    for result in parse_each(&files, cores) {
        match result {
            Ok(test_file) => parsed.push(test_file),
            Err(e) if explicit_file => return Err(e),
            // Printed directly rather than logged: dropping a file changes
            // what the run covers, so it must show up whatever the log level
            // happens to be. Escaped because the message carries the path the
            // directory walk turned up, and a file name is as much the tree's
            // choice as a test name is (see `output::escape_label`): raw, a
            // `touch $'x\e[2K.sh'` holding unparseable shell gets to redraw
            // the line that says it was skipped.
            Err(e) => errln!(
                "warning: {}; skipping this file",
                output::escape_label(&e.to_string())
            ),
        }
    }
    Ok(parsed)
}

/// How much of the machine a parse may use.
///
/// Not a question of taste. Parsing a tree is embarrassingly parallel, but
/// the threads that would do it are not free once they are done: creating one
/// leaves the process multi-threaded for good — glibc keeps the worker's
/// stack mapped for reuse and stops taking its single-threaded paths — and
/// the `fork(2)` behind every spawned test is measurably dearer afterwards.
/// Measured on a tree of 100 small files holding 500 tests, the spawns alone
/// went from ~390ms to ~450ms, which is ten times the 7ms the parallel parse
/// had saved there.
///
/// So the choice is made by what follows the parse, not by how big the parse
/// is: [`Cores::All`] where nothing is going to be forked per test,
/// [`Cores::One`] where a run is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cores {
    /// Take the machine. `list` and shell completion print what they found and
    /// exit, so the parse is the whole of their latency and nothing afterwards
    /// pays for the threads.
    All,
    /// Stay on this thread: a run follows, and it `fork`s once per test.
    One,
}

/// Parse every file in `files`, one result per file and in the same order.
///
/// Where the time goes once a tree holds scripts of any size:
/// `brush_parser` costs a few microseconds a line, the walk hands it every
/// script that could hold a test, and on 60 scripts of 450 lines the parse
/// was 95% of what `attest list` spent — all of it on one core. A file's
/// parse neither needs nor affects any other file's, so with [`Cores::All`]
/// they go out to as many workers as the machine has. The one thing two
/// parses share, the `PATH` lookup behind [`discovery::script_shell`], is
/// already answered once per shell name under a lock.
///
/// Order is restored afterwards rather than preserved by the scheduling:
/// files are claimed one at a time as workers come free, since a tree's
/// scripts differ in length by orders of magnitude and a fixed split would
/// leave whoever drew the big ones parsing alone.
fn parse_each(files: &[PathBuf], cores: Cores) -> Vec<anyhow::Result<parser::TestFile>> {
    let workers = match cores {
        Cores::One => 1,
        Cores::All => std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(files.len()),
    };
    if workers <= 1 {
        return files.iter().map(|f| parser::parse_test_file(f)).collect();
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut parsed: Vec<(usize, anyhow::Result<parser::TestFile>)> =
        Vec::with_capacity(files.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = (1..workers)
            .map(|_| scope.spawn(|| parse_claimed(files, &next)))
            .collect();
        // This thread is one of the workers: spawning `workers` of them and
        // then only waiting would leave a core to the scheduler's discretion.
        parsed.extend(parse_claimed(files, &next));
        for handle in handles {
            match handle.join() {
                Ok(done) => parsed.extend(done),
                // A worker panicking is this parse panicking: there is no
                // partial set of files to go on with, and swallowing it would
                // turn a bug into a run that silently covers less.
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
    });
    parsed.sort_by_key(|&(i, _)| i);
    parsed.into_iter().map(|(_, result)| result).collect()
}

/// Parse files out of `files` until they run out, each tagged with its index
/// so the caller can put the answers back in discovery order.
fn parse_claimed(
    files: &[PathBuf],
    next: &std::sync::atomic::AtomicUsize,
) -> Vec<(usize, anyhow::Result<parser::TestFile>)> {
    let mut done = Vec::new();
    loop {
        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(file) = files.get(i) else {
            return done;
        };
        done.push((i, parser::parse_test_file(file)));
    }
}

/// What a positional target plus an optional `--filter` select.
///
/// `list` and a run have to answer that identically: a listed selector is
/// documented to be re-runnable as a target, which only holds while both go
/// through the same split, the same discovery, the same tolerance for an
/// unparseable file and the same pattern match.
struct Selection {
    /// The path half of the target, kept for the "nothing found" message.
    path: PathBuf,
    /// `--filter`, or the name half the target carried.
    filter: Option<String>,
    /// The files that parsed, in discovery order. Each file is parsed exactly
    /// once and kept alive here, so every test can borrow its path and the
    /// script the runner sources rather than re-deriving either per test (and
    /// again per `--repeat`).
    files: Vec<parser::TestFile>,
}

impl Selection {
    /// Resolve `target` and `filter`: split the target, discover and parse what
    /// its path half names, and take the name half as the filter unless
    /// `--filter` gave one explicitly.
    ///
    /// `cores` only says how fast to get there, never what is found: the
    /// selection a run resolves and the one `list` prints have to be the same
    /// one (see [`Cores`] for why they ask for different amounts of machine).
    fn resolve(target: &str, filter: Option<String>, cores: Cores) -> anyhow::Result<Self> {
        let (path, inline_filter) = split_path_arg(target);
        let files = parse_discovered(
            discovery::discover_test_files(&path)?,
            path.is_file(),
            cores,
        )?;
        Ok(Self {
            path,
            filter: filter.or(inline_filter),
            files,
        })
    }

    /// The selected tests as `(function name, the file defining it)`, in
    /// discovery order.
    fn tests(&self) -> Vec<(&str, &parser::TestFile)> {
        let pattern = self.filter.as_deref().map(parser::TestPattern::parse);
        self.files
            .iter()
            .flat_map(|file| file.tests.iter().map(move |name| (name.as_str(), file)))
            .filter(|&(name, file)| pattern.as_ref().is_none_or(|p| p.matches(&file.path, name)))
            .collect()
    }

    /// Why nothing was selected: a filter that matched nothing is a different
    /// mistake from a target that holds no tests at all.
    fn nothing_selected(&self) -> anyhow::Error {
        match &self.filter {
            Some(f) => anyhow::anyhow!("no tests match filter '{f}'"),
            None => anyhow::anyhow!("no test functions found in {}", self.path.display()),
        }
    }
}

/// Which argument is being completed.
///
/// Both accept a whole `<file>/<test>` selector, and for that much they
/// complete identically. Anything short of one they read differently, so it
/// cannot be offered to both: a positional target is a *path* on its left
/// (`split_path_arg` only splits where that half is an existing file), while
/// `--filter` is a [`parser::TestPattern`], where an argument with no `/` in it
/// is all *name* and a file needs the trailing `/` to be the file half.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Completing {
    /// The positional target of a run or of `list`.
    Target,
    /// A `--filter` value.
    Filter,
}

/// Complete a positional target or a `--filter` value: the discovered script
/// files, plus the `<file>/<test>` selectors in them, that extend what has been
/// typed so far.
///
/// One path for both shapes of argument. All that the file/test split decides
/// is *where to look*: once the left half names an existing file, only that
/// file can complete the argument, and otherwise every file the walk turns up
/// can. From there both cases are the same prefix match against
/// [`parser::selector`], which is also what `list` prints and what
/// [`parser::TestPattern::parse`] reads back — so a completed selector names
/// the test it was completed from by construction rather than by two
/// open-coded `format!`s happening to agree.
///
/// They did not agree: a `./`-prefixed argument was completed by pasting the
/// name onto the path as typed, and behind a `./` a `--filter` is *all* path,
/// so accepting `./a/x.test/test_foo` asked for a file of that name and
/// selected nothing. The same `./` kept the file-name half from matching
/// anything at all, since the selectors it was compared against never carry
/// the marker.
///
/// What a completed *selector* means is the same for both, but what anything
/// shorter means is not (see [`Completing`]), and offering a target's
/// candidates for a filter handed back filters that select nothing:
/// `--filter testHel<TAB>` — the bare-name form that is the whole point of
/// `--filter` — offered nothing at all, because every candidate began with a
/// file path, and `--filter examples/md5<TAB>` completed to
/// `examples/md5sum.test`, which as a pattern is the *name* `md5sum.test` in
/// the directory `examples` and matches no test in the tree.
fn complete_tests(current: &std::ffi::OsStr, arg: Completing) -> Vec<CompletionCandidate> {
    let current = current.to_string_lossy();
    // What has been typed is compared against selectors, so it goes through
    // the same `./` rule they do rather than a second spelling of it.
    let typed = parser::selector_path(Path::new(&*current));

    let files = match split_file_and_name(&current) {
        Some((file, _)) => vec![PathBuf::from(file)],
        None => discovery::discover_test_files(Path::new(".")).unwrap_or_default(),
    };

    // A filter with no `/` left in it is all name, so a name is what is being
    // typed — and the prefix match below is the one `--filter` itself performs
    // on it (`parser::wildcard_match`).
    let name_only = arg == Completing::Filter && !typed.contains('/');

    // Spread over the machine, because nothing here forks afterwards: a tab is
    // a keystroke, and parsing the tree is the whole of what the wait is (see
    // `parse_each`).
    let parsed = parse_each(&files, Cores::All);

    let mut candidates = Vec::new();
    let mut offered_names = std::collections::HashSet::new();
    for (file, test_file) in files.iter().zip(parsed) {
        let path = parser::selector_path(file);
        if path.starts_with(&typed) {
            candidates.push(CompletionCandidate::new(match arg {
                Completing::Target => path.clone(),
                // The path alone is a name pattern to `--filter`; the trailing
                // `/` is what makes it the file half, and it also stops the
                // shell putting a space after a completion a test name still
                // has to be typed onto.
                Completing::Filter => format!("{path}/"),
            }));
        }
        let Ok(test_file) = test_file else {
            continue;
        };
        for name in &test_file.tests {
            let selector = parser::selector(file, name);
            if selector.starts_with(&typed) {
                candidates.push(CompletionCandidate::new(selector));
            }
            // A name can be defined in more than one file, and as a filter it
            // stands for every one of them, so it is offered once.
            if name_only && name.starts_with(&typed) && offered_names.insert(name.clone()) {
                candidates.push(CompletionCandidate::new(name));
            }
        }
    }
    candidates
}

fn complete_target(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    complete_tests(current, Completing::Target)
}

fn complete_filter(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    complete_tests(current, Completing::Filter)
}

/// Finish the way an ordinary Unix filter does once its reader has gone away.
///
/// Rust turns `SIGPIPE` off at startup, so restore the default disposition and
/// re-raise it: the shell then sees the usual signal status instead of an exit
/// code we invented, and `attest | head -1` behaves like `seq 100 | head -1`.
fn exit_broken_pipe() -> ! {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::raise(libc::SIGPIPE);
    }
    // Only reached if something had SIGPIPE blocked.
    std::process::exit(128 + libc::SIGPIPE)
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

    let mut any_failed = false;
    match cli.command {
        Some(Commands::Skill) => {
            output::write_out(include_str!("../SKILL.md").as_bytes());
        }
        Some(Commands::List { path, filter }) => {
            let selection = Selection::resolve(&path, filter, Cores::All)?;
            let tests: Vec<(&Path, &str)> = selection
                .tests()
                .into_iter()
                .map(|(name, file)| (file.path.as_path(), name))
                .collect();
            output::print_test_list(&tests);
        }
        None => {
            let path_arg = cli.path.unwrap_or_else(|| {
                errln!("error: a path to a test file or directory is required");
                std::process::exit(1);
            });

            let selection = Selection::resolve(&path_arg, cli.filter, Cores::One)?;
            let selected = selection.tests();
            if selected.is_empty() {
                return Err(selection.nothing_selected());
            }

            // How often each name occurs, so duplicates across files can be
            // given distinct display names.
            let mut name_counts: std::collections::HashMap<&str, usize> = Default::default();
            for &(name, _) in &selected {
                *name_counts.entry(name).or_default() += 1;
            }

            // Display names key result output, per-test context dirs, and
            // --save-context dirs, so they must be unique per test.
            let mut taken = std::collections::HashSet::new();
            let mut all_tests: Vec<runner::TestSpec> = Vec::new();
            for (name, file) in selected {
                let base = display_base(name, &file.path, name_counts[name] > 1, &mut taken);
                for i in 1..=cli.repeat {
                    let display = if cli.repeat > 1 {
                        format!("{base}#{i}")
                    } else {
                        base.clone()
                    };
                    all_tests.push(runner::TestSpec {
                        display,
                        name,
                        file,
                    });
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
                && !discovery::shell_exists(shell)
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
                no_cgroups: cli.no_cgroups,
            };

            let results = runner::run_all_tests(all_tests, &config)?;
            any_failed = results.iter().any(|r| !r.outcome.passed());
        }
    }

    // Our reader is gone, so the run has no report to exit about — exiting 1
    // for a failure nobody ever saw would be worse than useless under
    // `set -o pipefail`.
    if output::output_closed() {
        exit_broken_pipe();
    }
    if any_failed {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const GOOD: &str = "#!/bin/sh\ntestGood() {\n\ttrue\n}\n";
    // Unterminated double quote: valid for no shell. Spelled inside a test
    // function, so the file is one the parser is asked about at all (a script
    // that spells no test function is never parsed — see
    // `parser::may_define_test` and the test below).
    const BROKEN: &str = "#!/bin/bash\ntest_broken() {\n\techo \"unterminated\n}\n";
    // The same syntax error in the kind of script a tree can contain without
    // it having anything to do with the tests.
    const BROKEN_WITHOUT_TESTS: &str = "#!/bin/bash\nfoo() {\n\techo \"unterminated\n}\n";

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

        let parsed = parse_discovered(vec![broken, good.clone()], false, Cores::One)
            .expect("one bad script must not abort a directory run");

        // Only the file that parsed is kept, and it knows where it came from.
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path, good);
        assert_eq!(parsed[0].tests, ["testGood"]);
    }

    #[test]
    fn parse_discovered_is_fatal_for_an_explicitly_named_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let broken = script(tmp.path(), "broken.sh", BROKEN);

        let Err(err) = parse_discovered(vec![broken], true, Cores::One) else {
            panic!("a file the user named must not be skipped silently");
        };
        assert!(err.to_string().starts_with("parse error in "), "{err}");
    }

    /// Spreading the parse over the cores may change how long it takes and
    /// nothing else: every file still gets exactly one answer, in the order
    /// the walk turned it up, however the workers happened to pick them off.
    ///
    /// Asserted over a mix a tree really contains — files that parse, files
    /// that parse and hold nothing, files that do not parse — and over enough
    /// of them that the claims interleave, since with one file per worker the
    /// scheduling could never put an answer out of place.
    #[test]
    fn a_parse_spread_over_the_cores_answers_exactly_as_one_thread_would() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bodies = [GOOD, BROKEN, BROKEN_WITHOUT_TESTS];
        let files: Vec<PathBuf> = (0..60)
            .map(|i| script(tmp.path(), &format!("f{i:02}.sh"), bodies[i % bodies.len()]))
            .collect();

        let spread = parse_each(&files, Cores::All);
        let one = parse_each(&files, Cores::One);
        assert_eq!(spread.len(), files.len());
        assert_eq!(one.len(), files.len());

        for ((file, spread), one) in files.iter().zip(&spread).zip(&one) {
            match (spread, one) {
                (Ok(spread), Ok(one)) => {
                    // The answer is the one for *this* file, not merely some
                    // file's: a lost index would otherwise pass unnoticed as
                    // long as the multiset of answers came out right.
                    assert_eq!(&spread.path, file);
                    assert_eq!(&one.path, file);
                    assert_eq!(spread.tests, one.tests);
                    assert_eq!(spread.script, one.script);
                    assert_eq!(spread.shell, one.shell);
                }
                (Err(spread), Err(one)) => assert_eq!(spread.to_string(), one.to_string()),
                (spread, one) => panic!(
                    "{} parsed on {} threads but not on {}",
                    file.display(),
                    if spread.is_ok() { "many" } else { "one" },
                    if one.is_ok() { "one" } else { "many" },
                ),
            }
        }
    }

    /// A script that spells no test function at all is never handed to the
    /// parser, so its syntax is nobody's business — not even when the user
    /// named it. Whatever it holds, it holds no test: the answer the run gives
    /// is "no test functions found", which is both true and what was asked.
    #[test]
    fn a_script_that_can_hold_no_test_is_not_parsed_at_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let broken = script(tmp.path(), "broken.sh", BROKEN_WITHOUT_TESTS);

        for named in [true, false] {
            let parsed = parse_discovered(vec![broken.clone()], named, Cores::One)
                .expect("a script with no test in it must not fail a run");
            assert_eq!(parsed.len(), 1);
            assert!(parsed[0].tests.is_empty());
        }
    }

    /// A `./`-prefixed target keeps selecting the test it names.
    ///
    /// `./` in front of a filter means "all of this is a path", so the
    /// synthesized filter must not carry the marker over from the target: the
    /// name half the split just established would be read as the last
    /// component of a file path and match nothing.
    #[test]
    fn split_path_arg_drops_the_dot_slash_from_the_filter() {
        // `split_path_arg` decides the split from the filesystem, so a `./`
        // target needs a file that really is one relative to the working
        // directory — which cargo sets to the package root for a test binary.
        // Done against the repository's own example rather than a temp dir
        // because a relative path cannot name one, and chdir'ing to it would
        // race every other test in this binary.
        assert!(
            Path::new("examples/md5sum.test").is_file(),
            "expected to run from the package root"
        );

        let (path, filter) = split_path_arg("./examples/md5sum.test/testHello");
        assert_eq!(path, Path::new("./examples/md5sum.test"));
        assert_eq!(filter.as_deref(), Some("examples/md5sum.test/testHello"));

        // Both spellings of the target select the same test.
        let (_, bare) = split_path_arg("examples/md5sum.test/testHello");
        assert_eq!(filter, bare);

        let pattern = parser::TestPattern::parse(&filter.unwrap());
        assert!(pattern.matches(Path::new("./examples/md5sum.test"), "testHello"));
        assert!(!pattern.matches(Path::new("./examples/md5sum.test"), "testHelp"));
    }

    /// What completion offers for a positional target.
    fn completions(typed: &str) -> Vec<String> {
        candidates_for(typed, Completing::Target)
    }

    /// What completion offers for a `--filter` value.
    fn filter_completions(typed: &str) -> Vec<String> {
        candidates_for(typed, Completing::Filter)
    }

    fn candidates_for(typed: &str, arg: Completing) -> Vec<String> {
        complete_tests(std::ffi::OsStr::new(typed), arg)
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    /// Every test in the tree, as completion sees them: the same walk and
    /// parse, so a candidate can be checked against what it would select.
    fn discovered_tests() -> Vec<(PathBuf, String)> {
        discovery::discover_test_files(Path::new("."))
            .unwrap()
            .into_iter()
            .filter_map(|f| parser::parse_test_file(&f).ok())
            .flat_map(|tf| {
                tf.tests
                    .iter()
                    .map(|n| (tf.path.clone(), n.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Whatever is offered for `--filter` has to be a filter that selects
    /// something: `--filter` is a `TestPattern`, not a path, so the candidates
    /// a positional target gets are wrong there in both directions — a bare
    /// name prefix (the form `--filter` exists for) matched no selector and so
    /// was offered nothing, and a file path without the trailing `/` reads as
    /// the *name* pattern of its last component and matches no test at all.
    #[test]
    fn every_filter_completion_selects_a_test() {
        let tests = discovered_tests();
        assert!(!tests.is_empty(), "expected to run from the package root");

        for typed in [
            "",
            "test",
            "testHel",
            "examples/md5",
            "examples/md5sum.test/",
            "examples/md5sum.test/testHel",
        ] {
            let candidates = filter_completions(typed);
            assert!(!candidates.is_empty(), "nothing offered for `{typed}`");
            for candidate in candidates {
                let pattern = parser::TestPattern::parse(&candidate);
                assert!(
                    tests.iter().any(|(file, name)| pattern.matches(file, name)),
                    "`{typed}` offered `{candidate}`, which selects no test",
                );
            }
        }
    }

    /// A `/`-less `--filter` is all name, so that is what completing one
    /// offers — `list`'s selector form cannot extend it, since every selector
    /// starts with a file path.
    #[test]
    fn a_bare_filter_prefix_completes_test_names() {
        let candidates = filter_completions("testHel");
        assert_eq!(candidates, ["testHelp", "testHello"]);
    }

    /// A target keeps naming a path: a bare test name is not one, and offering
    /// it would complete to an argument that fails with "path does not exist".
    #[test]
    fn a_target_is_never_completed_to_a_bare_test_name() {
        assert!(completions("testHel").is_empty());
        assert!(
            completions("examples/md5").contains(&"examples/md5sum.test".to_string()),
            "a target still completes to the file path itself"
        );
    }

    /// A completed selector has to select the test it was completed from —
    /// under `--filter` as well as as a positional target.
    ///
    /// Behind a `./` a filter is *all* path (see `parser::TestPattern::parse`),
    /// so a completion that pasted the name onto the path as typed handed back
    /// `./examples/md5sum.test/testHello`: a file of that name, matching
    /// nothing. The selector form carries no `./`, so neither may a completion
    /// of it.
    #[test]
    fn a_completion_behind_a_dot_slash_still_names_its_test() {
        // Completion of the `<file>/<name>` shape is decided by the
        // filesystem, so this needs a file that really is one relative to the
        // working directory — which cargo sets to the package root.
        assert!(
            Path::new("examples/md5sum.test").is_file(),
            "expected to run from the package root"
        );

        let candidates = completions("./examples/md5sum.test/testHel");
        assert_eq!(
            candidates,
            [
                "examples/md5sum.test/testHelp",
                "examples/md5sum.test/testHello"
            ]
        );
        for candidate in &candidates {
            let pattern = parser::TestPattern::parse(candidate);
            let name = candidate.rsplit_once('/').unwrap().1;
            assert!(
                pattern.matches(Path::new("./examples/md5sum.test"), name),
                "the completion `{candidate}` selects no test"
            );
        }

        // The `./`-free spelling of the same prefix completes identically: the
        // marker only ever said "this is a path", and the split already
        // established that it is not all of one.
        assert_eq!(completions("examples/md5sum.test/testHel"), candidates);
    }

    /// Only the file named by the left half is consulted once there is one, so
    /// a selector cannot be completed out of some other file's tests (and the
    /// cwd is not walked to find out).
    #[test]
    fn a_completion_comes_only_from_the_file_its_prefix_names() {
        let candidates = completions("examples/md5sum.test/");
        assert!(!candidates.is_empty());
        assert!(
            candidates
                .iter()
                .all(|c| c.starts_with("examples/md5sum.test/")),
            "{candidates:?}"
        );
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

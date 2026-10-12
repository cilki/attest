## Architecture

- `src/main.rs` - CLI entry point using clap. Invoking `attest` with no
  subcommand runs the tests; `list` and `skill` are the only subcommands. The
  positional target is a file, a directory, or a `<file>/<test>` pattern, which
  is split into a path plus an implicit `--filter`. `list` prints each test as
  that same selector, keeping the directories the file was discovered under —
  the file half matches as a path suffix and a positional target only splits
  where its left half is an existing file, so a base-name-only listing is both
  ambiguous between same-named files and unusable as a target.
  `parse_discovered` parses every discovered file out of which the selection
  could pick a test (`parser::Prescan` settles that from the file's path and
  its text, without a parse): a
  parse error in a file the
  user named directly is
  fatal, while one in a file the directory walk turned up is reported on stderr
  and the file skipped, so an unparseable script elsewhere in the tree cannot
  abort the run. A file that spells no test function is never parsed at all,
  so it is never reported either — named or walked, it holds no test and that
  is the answer the run gives, and a `--filter` narrows that the same way:
  `Selection::resolve` builds the pattern before discovery, drops the files
  its file half cannot name and hands its name half to the scan, so the tests
  that come out are the same ones and almost none of the tree is parsed for
  them (5x on a run, 9x on a `list`, over 60 450-line scripts).
  `Selection::tests` still matches the whole pattern, since a `Prescan` only
  ever rules files *out*. How much of the machine that parse gets is
  `parse_each`'s `Cores` argument, and it is decided by what *follows* the
  parse rather than by how big it is: a file's parse is independent of every
  other file's, but creating a thread leaves the process multi-threaded for
  good and the `fork(2)` behind every spawned test is measurably dearer
  afterwards. So `list` and completion take every core (they print and exit,
  and the parse is the whole of their latency — 2.3x on a tree of 60
  450-line scripts) while a run keeps the parse on one thread, where the
  thousand-odd forks of a real suite would cost more than it saved.
  Whichever way it is spread, each file gets exactly one answer and the
  warnings come out on the main thread in discovery order. Shell completion is dynamic (`clap_complete`'s `CompleteEnv`,
  so `COMPLETE=<shell> attest` prints the completion script):
  `complete_tests` discovers script files and parses them to complete
  `<file>/<test>` pairs for the target and `--filter`. The file/test split
  decides only *where to look* — one named file, or everything the walk turns
  up — and from there completion is a prefix match against
  `parser::selector`, so a completed selector is the same string `list` prints
  and `TestPattern::parse` reads back. A whole selector means the same to both
  arguments, but nothing shorter does, so `Completing` says which one is being
  completed: a target is a *path* on its left (`split_path_arg` only splits
  where that half is an existing file), while `--filter` is a `TestPattern`,
  where an argument with no `/` is all name and a file needs the trailing `/`
  to be the file half. So the filter also completes bare test names (the form
  `--filter` exists for, which no selector can extend since every one of them
  starts with a file path) and completes a file to `a/x.test/` rather than
  `a/x.test` — which as a pattern is the name `x.test` under the directory `a`
  and selects nothing. A target gets neither: a bare name is not a path, and
  completing to one would hand back an argument that fails with "path does not
  exist".
- `src/discovery.rs` - Finds test files from a file path or directory
  (recursive). Scans all shell scripts (by extension or shebang) for test
  functions, not just `.test` files. Owns every rule about shebangs and
  shells, so the "is this a shell script" and "which shell does it want"
  answers cannot disagree: `shebang_shell` reads the interpreter out of a
  shebang line (`#!/usr/bin/env bash` included) and `script_shell` turns a
  script's *text* into the shell to exec it with — the shebang's shell if it
  is installed, `/bin/sh` if it is not, `bash` when there is no recognized
  shebang. It takes the text and not the path because by then the file has
  been read whole and parsed anyway; the runner used to reopen and re-read it
  for the first line on every spawn, once more for every `--repeat`.
  `resolve_shell` remembers what each shell name resolved to: every file the
  walk turns up asks the same question about the same handful of shells, and
  a bare name costs a `which` — a stat per `PATH` entry, which on a tree of
  scripts holding no tests was most of what the walk spent.
- `src/parser.rs` - Parses shell scripts with `brush_parser::Parser`, walks the
  AST to extract all `FunctionDefinition` nodes. Test functions start with
  `test`. A `TestFile` keeps the names of its tests, the definitions the runner
  puts at the head of the script it runs (`build_functions_source`) and the
  shell that runs it (`discovery::script_shell`), not the nodes: all of those
  are the same for every test in the file, and rendering the script walks each
  function's AST for its source span — which cost more than reading the file it
  is built from, and a file holding 25 tests paid it 25 times over, once more
  for every `--repeat`. Deriving them at parse time also means the script, the
  spans it is cut with and the shell all come from one read of the file, so
  they cannot disagree. A file is only parsed at all once `Prescan` has found
  the shape of a selectable definition in its text: the shell parse
  costs about a microsecond per line, the walk hands it every script in the
  tree, and almost none of them define a test. A file that fails that scan
  gets an empty `tests` and an empty `script` — nothing can source a script
  none of whose tests can be selected. The scan may only ever err towards the
  parse: a false positive costs what would have been paid anyway, while a
  false negative loses a test. A `Prescan` is the whole of what a selection
  rules out before a parse, so it carries the `--filter` as well as the
  `test…` rule: the pattern's file half is a path suffix (`admits`, the same
  `Path::ends_with` the match itself uses) and its name half fixes the literal
  prefix the definition's name must carry — everything in front of the first
  `*`, or all of a `*`-less pattern, since that matches as a prefix. One of
  that and `test` has to contain the other, so the longer is the whole
  constraint and a name half that extends neither (`--filter overlay`) means
  no file in the tree holds a selectable test at all. The price of a
  rejection is the same one the bare `test…` scan already pays: an
  unparseable script the filter excludes is no longer named on stderr, and
  neither is a test such a file defines twice. Also holds `TestPattern`, the `[<file>/]<name>` selector behind
  `--filter` and the positional target: the file half is a `Path::ends_with`
  suffix (whole components, no wildcards), the name half a `*` glob that
  degrades to a prefix match when it contains no `*`, and either half may be
  empty to mean "unconstrained". Without a `/` the whole pattern is a name, so
  a file-only filter needs the trailing slash — except behind a `./`, which
  marks the whole argument as a path: `parse` strips the marker and takes
  everything after it as the file, however many components that is
  (`./a/x.test` == `a/x.test/`). The marker wins over the last-`/` split rather
  than only applying when there is nothing to split, so a name is attached by
  spelling the selector without the `./` (`a/x.test/test_foo`, the form `list`
  prints). `split_path_arg` therefore drops a `./` from the filter it
  synthesizes for a positional target: there the split is decided by the
  filesystem, so `./a/x.test/test_foo` is a file plus a name, and carrying the
  marker into the filter would turn the name back into a path component.
  `selector` is the other direction — the single spelling of the join, used by
  `list` and by completion — and drops the `./` for the same reason:
  `TestPattern::parse` would otherwise read the whole selector as a path.
  `parse_test_file` also ties the file's `#attest(...)` blocks to its tests
  (`attach_decorators`): brush drops comments in the tokenizer, so the blocks
  come from `decorator::scan` over the text and are attached to the function
  whose `location()` starts below each, across any comment lines in between
  (a doc comment usually sits there). A block that nothing follows, that a
  blank or a line of code separates from its function, that decorates a
  non-test function, or that uses a form nothing enforces yet is a parse
  error of the file: an assertion the run would silently never check is worse
  than a loud one. The scan runs *before* the shell parse so that the
  likeliest mistake — continuation lines without their `#` — is named as an
  unclosed block rather than as the shell's syntax error. The lines from a
  block down to its function are the one exception to "only definitions are
  emitted": `build_functions_source` copies them into `script` at their own
  line numbers instead of blanking them (they are comments, so the shell runs
  nothing more) because a violation's snippet renders the script, and has to
  show the condition as written.
- `src/decorator.rs` - The `#attest(...)` grammar and its evaluation. A
  block opens on a line that is exactly one `#` (`##` is a doc comment), blanks,
  `attest`, blanks, `(`, and closes at the first `)` outside braces, across
  any number of comment lines whose `#` is stripped; `scan` returns every block
  with its lines, and `parse_conditions` splits the joined text at the commas
  outside braces (trailing comma allowed, empty block or empty condition an
  error) and keeps each condition's line and byte span so the report can
  underline exactly it. A condition is `METRIC OP QTY`, `QTY OP METRIC` or
  `QTY OP METRIC OP QTY` (one metric, a chain pointing one way, a non-empty
  range), with the unit family fixed by the metric (`Unit`: durations in
  microseconds, sizes in bytes, counts bare) — or one of the set forms:
  `syscalls in`/`not in {…}` (each name resolved against this architecture's
  table through the `syscalls` crate at parse time, so a typo is a parse
  error and `Kind::Syscalls` holds `Sysno`s; `watched_syscalls` is the set
  the run has to listen for, the set itself for `not in` and its complement
  for `in`), `writes in {…}` (each item `/absolute` or `./relative`, no
  `..` — a bare `build` would mean one thing or the other depending on who
  is asked), and `hosts in {…}`, which parses but is refused by
  `check_supported` until something enforces it. `Condition::check` judges
  a condition against a `Sample`: the `wall` duration, the cgroup's
  `ResourceStats` (`cpu` is user+system, the number the resource line
  shows), the `SyscallLog` the seccomp listener built, the delta's paths and
  the invocation dir a `./` item is relative to — each half `None` where the
  run has no such number, which makes a condition on it `Unmeasured` rather
  than passed. The outcome is `Ok`, that, or a `Violation`: line, span,
  condition text, a message (`cpu was 12.2ms`, `made socket, connect`,
  `wrote /etc/motd (+3 more)`) and a `Detail` — for a quantity the metric,
  the measurement and the `Bound` it broke; for a set the full list of
  offenders, since the message spells only `SHOWN_OFFENDERS` of them. Each
  `Bound` carries its `Literal` — the number token's line, byte span and
  text — recorded in `parse_operand` as an offset into the condition and
  mapped onto the file's lines in `parse_conditions`; a chain written across
  lines has its two literals on two lines, which is why the position is per
  bound. That literal is the whole of what `--update-decorators` rewrites.
  `Measure` is the per-measure identity behind `warn_unmeasured`: the six
  metrics plus `Syscalls` and `Writes`, each with the source that failed to
  report it.
- `src/decorate.rs` - `--update-decorators` and `--add-decorators`: planned
  from the `TestResult`s a finished run returns, applied once after it, never
  reached by an interrupted run. `bound_for` is the one rule for a new number:
  the extreme sample (max for an upper bound, min for a lower — this is how
  `--repeat` folds) with headroom, ×1.25 rounded up or ×0.8 rounded down to
  two significant figures in the base unit, so it reads as a chosen number and
  never lands on a sample; a count is exact; whatever came out must admit
  every sample or the exact bound past the extreme is used instead, and the
  one bound nothing satisfies (`> x` under a sample of 0) is `None` and
  skipped with a warning. `plan_updates` groups violations by the literal's
  `(path, line, column)` and emits a `Replace` of that token only; a bound
  that held is never touched, and nothing but the number is. A set violation
  (`Detail::Set`) has no number to loosen — which syscalls or paths a test
  may use is a decision — so it is named in a warning and left as written.
  `plan_additions`
  writes an `Insert` of `# attest(cpu < …, mem < …, pids <= …)` (the
  `Metric::GENERATED` subset the run measured — `wall` is scheduler noise and
  `io_*` rarely the point) directly above the definition line
  (`TestFile::test_lines`, so a test the parser gave no span for gets none),
  at the definition's indentation, for a test with no block
  (`TestFile::has_decorator`) whose every run's commands succeeded. Directly
  above the definition rather than above a doc comment, because the line
  above a definition is known to be its own while a comment further up may be
  the previous function's. `apply` reads each file once, checks every
  `Replace`'s old text is still where the parse put it (a file edited during
  the run is skipped with a warning, not guessed at), applies bottom-up and
  right-to-left so no edit moves another, and `fs::write`s in place so the
  inode and the `+x` bit survive. The summary lists each edit with the file
  relative to the cwd, on stdout, or on stderr under `--json`.
- `src/runner.rs` - For each test: forks a child (through `spawn::Spawn`,
  not `std::process::Command` — see `src/spawn.rs`) that execs `<shell> -c`
  (the shell the file's own parse settled on) with the original
  script path as `argv[0]` so `$0` still works. The script it is given is all
  the file's extracted functions (test + helper) followed by `set -e`, a `cd`
  into the test's clean working directory and only then `set -x` before the
  test function is invoked by name — so the xtrace log starts at the test
  itself rather than at the function definitions. The definitions are carried
  in the script rather than sourced out of a file beside it: `.` is the one
  command here a shell need not have, and attest runs whatever shell a
  shebang names (issue #93). Nothing is emitted above them, since they are
  rendered at the line numbers they have in the test file and `$LINENO` counts
  from the first line of the script the shell is running — which is the whole
  of how `diagnostics` maps a trace line back to the source. The cost is that
  a file with more than about half a megabyte of function definitions in it
  cannot be exec'd at all (`E2BIG`, which the spawn error explains). The logs
  are opened by the parent and handed to the child as its stdout and stderr
  rather than redirected by the script, so a shell that will not parse the
  definitions — and so never runs a line of the script — still complains into
  `xtrace.log` instead of over attest's own output. The
  child pivots into a per-test ephemeral root (see `src/overlay.rs`) when
  available, runs in its own
  session (so timeouts and ^C kill the whole process tree via its process group
  and, when cgroups are active, `cgroup.kill`). Parallel by default via
  `fork(2)` with configurable parallelism (`--parallel`). The poll loop that
  reaps the children and spawns their replacements waits on `SIGCHLD`
  (`ChildWaiter`, blocking it for the length of the run and reading it
  through a `signalfd`) with the 50ms poll interval only as a timeout, so a
  finished test is noticed and replaced at once instead of on the next tick
  — that interval used to be a floor under every test's duration. The wait
  is one `poll` over that descriptor and every running test's seccomp
  listener (`PendingTest::listener`, see `src/seccomp.rs`), answering
  notifications as they arrive: a test blocked in a watched syscall must not
  wait for the next tick either. The mask is emptied again in the child's
  first hook, since a shell that inherited a blocked `SIGCHLD` would not see
  its own background jobs finish. The hooks run in the child in this order:
  session and mask, cgroup placement (in the host namespace, before any
  unshare), the ephemeral root, and last the seccomp filter for a test with
  a `syscalls` condition — after the mounts, so attest's own are not in the
  count. `watched_syscalls` is the union over the test's conditions; the
  listener comes back over a socketpair before the exec, and the spawn then
  waits for the exec with that listener in hand (`Spawning::wait_exec`),
  since the exec may itself be the first syscall notified. Supports `--timeout`,
  `--bail`, `--override`, `--bin-dir`, and `--strace`. With `--save-context`,
  copies each test's filesystem delta plus logs to the output dir. Display
  names (and thus context dirs) are unique per test: a test name defined in
  more than one file is qualified as `file.test:test_name`, and `--repeat`
  appends `#<n>`. Both are report labels, not selectors — `TestPattern` matches
  the function name — so the README documents them next to the filter grammar
  they cannot be used with; keep the two in step.
  `build_result` is where a test's `#attest(...)` conditions are checked,
  since it is the one place that has the outcome, the wall duration and the
  cgroup's stats together: only a run whose commands all succeeded is judged
  (a failed or timed-out run's numbers are those of a test that never got to
  its end), a violated condition turns `Pass` into `Fail` and goes on
  `TestResult::violations`, and a metric the run has no number for —
  `--no-cgroups`, no cgroup v2, a controller not enabled, seccomp
  notification unsupported — is reported once per run per `Measure` on
  stderr (`warn_unmeasured`) and leaves the outcome alone, since a condition
  on a number nobody has is not one the test can fail. The delta
  (`overlay::delta_paths`) is only walked for a test with a `writes`
  condition, and the syscall log is whatever the listener heard, taken off
  it here (`TestResult::syscalls`, which `--json` reports) whatever the
  outcome. `commands_succeeded` is what the report loop reads: a violation-only
  failure gets `diagnostics::print_violations` and no xtrace replay, because
  there is no failed command in the trace to replay or to blame. Under
  `RunConfig::update_decorators` a violation is recorded but the outcome
  stays `Pass` — the run exists to find the right bounds. `TestResult::test`
  is the function's name as written (the display name may be qualified and
  numbered), which is what `decorate` keys additions by.
  Every context lives under the run's one temp dir, which
  `private_temp_dir` creates mode `0700`: a context holds the test's working
  directory, every file it wrote in its ephemeral root, and an `xtrace.log`
  of each command with its arguments already expanded, so whatever secret a
  test handled is in there verbatim — and `tempfile` gives a temporary
  *directory* the plain umask (`0755` in practice) under the shared `/tmp`,
  unlike a temporary file. The per-test dirs underneath keep the umask: a
  private parent cannot be traversed, so nothing inside it is reachable
  either way. `--save-context` is deliberately left alone — that is a path
  the user named, and a copy they asked for.
- `src/overlay.rs` - Per-test whole-root overlayfs isolation. Each test pivots
  into a private copy-on-write view of `/`: writes land in per-test upper
  layers and are discarded on exit. The project mount (the one holding the
  invocation dir) and the scratch dirs `/tmp`/`/var/tmp` get their own
  ephemeral overlays — the scratch dirs whether or not they are mount points,
  so the host's partitioning cannot decide whether writes to them are
  discarded; every other mount (`/proc`, `/dev`, `/sys`, file binds
  like `/etc/resolv.conf`, …) is recursively bind-mounted through live and so
  stays shared with the host — writes there persist. Overlays need
  `CAP_SYS_ADMIN`: either directly (`Mode::Privileged`, unshare a mount
  namespace) or via a user namespace (`Mode::Userns`, which also maps the
  caller to uid 0 so the test sees itself as root in its namespace). A private
  UTS namespace keeps hostname changes inside the test.
  `probe_support` rehearses the full setup (same options, same `pivot_root`)
  once per run in a throwaway child; if no mode works the run does not start
  — there is no running a test outside an overlay — and the error
  (`Unsupported`) names the `Stage` each mode tripped on and its errno, which
  the probe child reports back over a pipe since the exit status has no room
  for both. A per-test setup failure after a successful probe fails the run
  loudly rather than silently running unisolated. The runner's unit tests
  follow the same rule: `overlay_env`/`run_script` return `None` where no
  overlay can be mounted and the test returns early, since there is nothing
  else to run a test in. All
  mounts happen in the forked child's private mount namespace via a spawn hook,
  so they are torn down automatically on exit and never pollute the host.
  The test's own context dir is bind-mounted back in **live** (the logs are
  written and the working directory lives there), so a running test can rewrite
  it: everything attest reads back out of a context — `stdout.log`,
  `xtrace.log`, the `upper`/`cwd`/`strace` dirs — goes through
  `open_nofollow`/`is_real_dir`/`copy_dir_recursive`, which refuse a symlink
  standing at the name instead of resolving it to some host path.
  `open_nofollow` also insists (`O_NONBLOCK` + an `fstat` on the descriptor it
  got) that the name is still a regular file: a read-only open of a FIFO blocks
  until a writer appears and `std` retries it across `EINTR`, so a test that
  replaced one of its logs with one used to wedge the run in `open(2)` past
  even `^C`.
  `copy_dir_recursive` reproduces a file's permissions but never its
  set-user-ID/set-group-ID/sticky bits (`copy_file_without_setid`, which is why
  it is not `fs::copy`): a delta is whatever the test wrote, the copy is owned
  by whoever ran attest — root under `sudo`, which is how overlays usually get
  mounted — and `--save-context` puts it in a directory of the user's choosing,
  so a `chmod u+s` inside the sandbox must not come out of it as a setuid
  binary anyone with access to the saved context can run. The mode is set on
  the descriptor before any content is written, and set explicitly rather than
  left to `O_CREAT`, so a name an earlier run left setuid is corrected.
  `delta_dirs` is the one listing of the upper layers a test's writes landed
  in (the root's at `/`, each ephemeral submount's at its mount point), read
  by both `--save-context` and `delta_paths`, so the two cannot cover
  different layers. `delta_paths` turns the layers into the absolute paths a
  `writes` condition is judged on: every non-directory entry (files,
  symlinks, the whiteout device a deletion leaves) is a write, and a
  directory is one only when it is the test's own — absent from the lower
  layer at that path, or marked opaque (`is_opaque`, the `trusted.`/`user.
  overlay.opaque` xattr) because the test replaced it — in which case it is
  listed once and not walked. A directory that exists below and is not
  opaque is one overlayfs copied up to hold a write further down, and only
  what is further down counts.
- `src/seccomp.rs` - Watching a test's syscalls with seccomp user
  notification. `Filter::watching` builds the classic-BPF program: allow any
  other ABI unseen (`NATIVE_ARCH` is the audit arch the numbers were resolved
  against), then one compare-and-return per watched number so no jump
  reaches further than the next instruction (a jump offset is a byte, and an
  allow-list's complement has hundreds of entries), `USER_NOTIF` for a hit
  and `ALLOW` otherwise. `install_and_send` runs in the child's last hook:
  `seccomp(SET_MODE_FILTER, NEW_LISTENER)` — with `CAP_SYS_ADMIN`, which
  the overlay already needed, or after `PR_SET_NO_NEW_PRIVS` where that is
  refused — and `SCM_RIGHTS` the listener over the socketpair, stack buffers
  only. `Listener::service` answers every pending notification (`NOTIF_RECV`,
  record the number, `NOTIF_SEND` with `FLAG_CONTINUE`, so the syscall runs
  exactly as it would have) and only ever reads while `poll` says the
  descriptor is readable, since `NOTIF_RECV` blocks otherwise; `POLLHUP`
  marks it dead, which is nobody left behind the filter. `probe` rehearses
  the whole round trip once per run (`getppid` through a filter in a forked
  child), and where it fails the run installs nothing and the condition is
  `Unmeasured`.
- `src/spawn.rs` - Fork-and-exec with the parent kept awake. `std::process::
  Command` blocks in `spawn` until the child's `execve` returns — and under
  a filter watching `execve` that return waits on the parent answering the
  notification, so each would wait on the other. `Spawn` does what `std`
  does (hooks in order in the child, a close-on-exec pipe carrying a failing
  hook's errno, EOF meaning the exec went through) but `Spawning::wait_exec`
  polls that pipe together with whatever descriptors the caller hands it,
  servicing them until the outcome arrives. `Child` is the pid plus whether
  it has been reaped, with the `kill`/`try_wait`/`wait` the runner used from
  `std`.
- `src/diagnostics.rs` - On failure, parses `xtrace.log` to find the last
  executed command (skipping `++` subshell traces, so a failure inside
  `$(...)` is attributed to the statement around it), maps it back to the
  original source file, and renders an annotate-snippets error snippet. Only
  a trace line inside one of the file's functions can be the test's own: the
  script is nothing but those definitions at their own line numbers plus one
  call, which is always the first trace line. Any other top-level line is an
  `EXIT` trap action — bash runs the trap string after unwinding the function
  with `$LINENO` restarted at 1, so `trap 'rm -rf "$tmp"' EXIT` traces as
  `+1: rm -rf …` after the failure and used to put the snippet on the shebang
  — and `parse_xtrace_failure` stops there, so the failure stays on the last
  command of the body even when the trap goes on to call a function whose
  lines are in the file. (`DEBUG`/`ERR` actions trace at `++` depth and are
  already skipped as subshells.)
  Also shows `left`/`right` operands for a failed `[`/`test`/`[[` comparison
  and, for `=`/`==`/`!=`, an inline character-level diff of them. `[` and
  `test` are one builtin under two names, and xtrace renders them the same way
  but for the `'['`/`']'` arguments, so `parse_bracket_expr` reads both through
  the same quoted-word split — a `test` spelling must not be the one that gets
  no operands back. The report the
  whole of this adds up to is documented in the README, so keep both in step
  when changing it.
  The log is scanned a line at a time, keeping at most `MAX_LINE` bytes of any
  one of them, rather than read whole: the failing command's own output sits
  between its trace line and the end of the file, so the trace cannot be found
  by reading a tail — but the output it has to be found behind is as big as the
  test made it, and need not contain a newline at all.
  The lines a snippet renders, and the function bounds that clamp its window,
  come from the `parser::TestFile` the test was run from (`script` and
  `functions`, which `build_functions_source` records as it lays each
  function out) rather than from reading the file back: the xtrace line
  number is `$LINENO` of the script the shell ran, so that script is the text
  it indexes, and it is already in memory for every test of the file. The
  report used to re-read the source and parse it a second time for the bounds
  — once per file per run, behind a cache — which was a second parse of text
  the discovery parse had already laid out, and bounds for the *file* rather
  than for the script wherever the two differ (a function the parser gave no
  span for is re-rendered from its AST at some other line).
  The source lines the snippet renders go through `escape_c1` first:
  `annotate_snippets` substitutes the C0 controls in the source it is given
  but prints `U+0080`–`U+009F` through, and those are controls a UTF-8
  terminal obeys (`U+009B` is `CSI`). Escaping happens before the annotation
  span is measured, since it moves the offsets the span is built from.
  `print_violations` is the report for a test that failed only its
  `#attest(...)` conditions: one snippet per violation, rendered from the same
  `script` (the parser keeps decorator lines in it at their own line numbers
  for exactly this), with a `Highlight` that underlines just the condition's
  byte span — `cpu < 10ms` inside `#attest(cpu < 10ms, pids <= 3)` — and
  carries `cpu was 12.2ms` as the annotation's label. The window runs from
  the condition to the end of the function below it, so the test's name is in
  view; the decorator sits outside every function bound, so the clamp is
  found as the first function starting under it.
- `src/output.rs` - ANSI-colored terminal output for PASS/FAIL and summary, plus
  the `Palette` that decides whether those sequences are written at all. A
  styled write takes them from the palette of the stream it is bound for
  (`out_palette`/`err_palette`, decided once per stream) rather than spelling
  them inline, so `Palette::PLAIN` empties the whole report at once: off a
  terminal the same `format!` writes the same words with nothing around them,
  which is what `attest . > results.txt`, a CI log and `grep '^FAIL'` want.
  `colors_wanted` is the rule — `NO_COLOR` off, then `CLICOLOR_FORCE` on, then
  the stream's own `isatty` with `TERM`/`CLICOLOR` — and it is `anstream`'s
  precedence on purpose, since clap's own usage errors already answer to those
  variables. The two streams are asked separately because the halves of a
  failure report are separately redirectable: the `FAIL` line and the snippet
  go to stdout, the xtrace replay to stderr. The snippet renderer takes the
  verdict rather than the sequences (`Renderer::styled`/`plain` is
  annotate-snippets' own switch), and
  `--json` carries no styling either way. Also holds
  the `--json` JSONL encoder (`print_test_result_json`): one object per test with
  `name`, `file`, `status` (`pass`/`fail`/`timeout`), `duration_ms`, `stdout`,
  `xtrace`, `strace`, `resources` and `violations` (always a list; a decorator
  failure is a plain `fail` with the detail there). The schema is documented in
  the README, so keep both in step when changing it.
  Test names and discovered file names go into a report line through
  `escape_label`, which spells their control characters `\xNN`: both are
  arbitrary bytes from the tree (a shell takes almost anything as a function
  name), and raw they let a failing test move the cursor back over its own
  `FAIL` and print a `PASS` instead. The C1 block (`U+0080`–`U+009F`) counts as
  control there, not as text: a terminal in UTF-8 mode decodes those code
  points and acts on them, and `U+009B` is `CSI` — an exact stand-in for
  `ESC [`. `--json` is unaffected and reports names verbatim.
  Every write to stdout/stderr goes through `write_out`/`write_err` (and the
  `outln!`/`errln!` macros) rather than `println!`: Rust disables `SIGPIPE`, so
  `println!` panics when the reader goes away (`attest | head -1`). Instead the
  first `EPIPE` latches `output_closed()`, the runner winds the run down like an
  interrupt, and `main` re-raises `SIGPIPE` so the shell sees the usual status.
  A captured log goes out through `replay_log_err` instead, which escapes every
  C0 control byte but `\n`/`\t`, plus the two-byte UTF-8 spelling of a C1
  control (`c2 80`–`c2 9f`): `xtrace.log` carries whatever the test wrote to
  stderr, and replaying that raw would let a cursor-movement sequence in it
  erase and rewrite the report around it. Raw bytes in `80..=bf` stay as they
  are — most of them are the continuation bytes of ordinary non-ASCII text, and
  alone none of them is valid UTF-8 for a C1 control. It replays from a reader a
  bounded chunk at a time rather than taking the log as one buffer: a log is as
  big as the test that wrote it chose to make it, and holding one whole — plus
  the escaped copy, up to four bytes per byte — is how one noisy failing test
  used to get the whole run OOM-killed. A trailing `c2` is held back until the
  byte after it arrives (the next chunk, or the next tail of a `-vv` stream)
  rather than sanitizing half a control, and the dimming the replay is wrapped
  in is only emitted if there is anything to put in it. `--json`/`--save-context`
  do no escaping and still carry the bytes verbatim, but they are bounded the
  same way: `--save-context` copies a log with `io::copy`, and `--json`
  serializes each result straight to stdout (`write_test_result_json`, a
  `serde::Serialize` impl over a `BufWriter<StdoutSink>`) instead of building a
  `serde_json::Value` first. The `stdout`/`xtrace`/`strace` fields are
  `LogField`s, which `collect_str` a chunk at a time out of `open_nofollow`, so
  no log is ever held whole — building the `Value` meant holding the log, the
  escaped JSON copy of it and the serialized line all at once, which is the same
  OOM one noisy test used to cause on the terminal path. Lossy decoding happens
  per chunk, so a character straddling a read boundary is held back rather than
  being decoded as two `U+FFFD`s (`write_lossy`).

## Key dependencies

- `brush-parser` - Tokenizes and parses shell scripts into an AST
- `clap` - CLI argument parsing
- `tempfile` - Per-test temporary directories
- `syscalls` - This architecture's syscall table (`Sysno`), for resolving
  the names in a `syscalls` condition and naming what the listener heard

## Test file format

Standalone test files (`.test`) have any shell shebang and contain functions
prefixed with `test`. Test functions can also be inline in any regular shell
script. When scanning directories, all shell scripts (identified by extension or
shebang) are checked for test functions; the walk is recursive, skips hidden
files and directories, and never follows symlinks (a symlinked directory would
pull in — and run — tests from outside the tree, and one pointing at an ancestor
would yield each test once per level), while a path passed directly is always
scanned whatever it is named. Each test runs under the shell named by its file's shebang, falling
back to `/bin/sh` when that shell is not installed and to `bash` when there is
no recognized shebang. Every command
in a test function is an implicit assertion - if it exits nonzero, the test
fails. Non-test functions (helpers/setup) are also extracted and made available
to tests.

Only function definitions are extracted (`build_functions_source` drops
everything else), so top-level code in a test file never runs: globals, `trap`s
and `set` options have to live inside a function. The generated runner script
enables `set -e` and `set -x` and nothing more — `pipefail` in particular is
off, so a test asserting on a pipeline must set it itself (see
`examples/race_condition.test`).

A comment block of the form `# attest(cond, cond, ...)` directly above a test
function (comment lines, a doc comment included, may sit between) is a
decorator: each condition compares one metric of the run — `cpu`, `wall`,
`mem`, `io_read`, `io_write`, `pids`, with the unit family the metric fixes —
against a value with `<`, `<=`, `>`, `>=`, or brackets it (`2ms < cpu <
10ms`). The block may span several `#` lines and take a trailing comma; blanks
are allowed around `attest`; `##` never opens one. A violated condition fails
the test once its commands have all succeeded (see `examples/md5sum.test`,
and the README's "Decorators"). `syscalls in`/`not in {…}` asserts on the
system calls the test's whole tree made (watched through seccomp user
notification, the shell's startup included), `writes in {…}` on the paths
its filesystem delta touched (`/absolute` or `./relative` to the invocation
dir); `hosts in {…}` parses but is rejected until enforced.
`--add-decorators` writes such a block, and `--update-decorators` rewrites
a bound's number when the run breaks it (a set condition has none, and is
left alone with a warning).

## CLI options

- `--parallel N` — max concurrent tests (default: CPU cores)
- `--timeout SECS` — wall-clock timeout per test; timed-out tests show `TIME`
  and count as failures
- `--bail` — end the run at the first failure. `bail_flag` stops new tests
  being spawned and makes the poll loop drop each in-flight test instead of
  turning it into a result, so the summary totals only the tests that finished.
  It does not kill them: the loop keeps `reap`ing, so every abandoned test runs
  to its natural end first (`PendingTest::drop` only reaches `kill_tree` for
  stragglers in its process group) and the run lasts as long as the slowest one
  — unbounded without `--timeout`, which is the only thing that kills on a
  deadline. `^C`/`SIGTERM` and a closed output stream are the paths that do
  kill: they `clear()` the pending list outright, and `^C` bails out without a
  summary
- `--filter [FILE/]PATTERN` — run only matching tests. `FILE` is a path suffix
  (no wildcards); `PATTERN` takes `*` wildcards and matches as a prefix only
  when it has none. Without a `/` the argument is all `PATTERN`; behind a `./`
  it is all `FILE`, however many components it has (`./a/x.test` ==
  `a/x.test/`)
- `--override SPEC` — copy a binary into the test context `bin/` dir so tests
  use it exclusively. SPEC is either a path (`/usr/bin/example` or
  `./bin/example`) or a mapping (`example=/usr/bin/override`)
- `--bin-dir DIR` — prepend DIR to each test's PATH so bare-name calls resolve to
  executables in DIR (no copy). Lower precedence than `--override`, higher than the
  inherited PATH. Repeatable
- `--strace CMD` — wrap CMD with strace, output saved to `strace/CMD.log` in the
  test context dir. Requires `strace` on PATH; the run aborts if it is missing.
  Pair with `--save-context` to keep the logs
- `-v`, `--verbose` — increase verbosity (repeatable). By default only the
  progress bar is shown, plus a FAIL line with the test's xtrace output and a
  diagnostic snippet for each failure; `-v` adds per-test PASS/FAIL lines;
  `-vv` also streams xtrace output live (one test at a time)
- `--save-context DIR` — for each test, copy the files it created/modified
  (all overlay upper layers merged, laid out by absolute path: a write to
  `/tmp/x` appears at `DIR/<test>/tmp/x`) plus the test's working directory at
  `DIR/<test>/cwd/` (it is live-bound, not part of any overlay),
  `stdout.log`/`xtrace.log` to `DIR/<test>/`, and any `--strace` logs at
  `DIR/<test>/strace/CMD.log` for debugging
- `--repeat N` — run each test N times (default: 1); combine with `--fuzz` to
  shake out flaky tests
- `--fuzz [VALUE]` — randomly pause and resume each test's descendant processes
  (`SIGSTOP`/`SIGCONT`) to introduce timing non-determinism. Optional
  aggressiveness in (0,1); higher pauses more often (default: 0.5). The
  argument after a bare `--fuzz` is only taken as its value when it reads as a
  number (`bind_bare_fuzz` rewrites the flag to `--fuzz=0.5` otherwise), so
  `attest --fuzz <target>` leaves the target alone
- `--json` — print results as JSONL (one JSON object per test) instead of the
  colored terminal output
- `--no-cgroups` — disable cgroup resource tracking
- `--update-decorators` — a violated `#attest(...)` bound does not fail the
  test; after the run the literal is rewritten to admit what was measured,
  with headroom. `--add-decorators` — after the run, a block bounding `cpu`,
  `mem` and `pids` is inserted above every test that has none. Both fold
  `--repeat`, write once after a finished run, and are independent (see
  `src/decorate.rs`)
- `-d`, `--debug` — enable debug logging

## Building and running

Use `cargo run --` to execute the project.

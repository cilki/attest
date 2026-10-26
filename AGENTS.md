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
- `src/runner.rs` - For each test: forks a child that execs `<shell> -c` (the
  shell the file's own parse settled on, or `--shebang`) with the original
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
  (`ChildWaiter`, blocking it for the length of the run) with the 50ms poll
  interval only as a timeout, so a finished test is noticed and replaced at
  once instead of on the next tick — that interval used to be a floor under
  every test's duration. The mask is emptied again in the child's `pre_exec`,
  since a shell that inherited a blocked `SIGCHLD` would not see its own
  background jobs finish. Supports `--timeout`,
  `--bail`, `--override`, `--bin-dir`, and `--strace`. With `--save-context`,
  copies each test's filesystem delta plus logs to the output dir. Display
  names (and thus context dirs) are unique per test: a test name defined in
  more than one file is qualified as `file.test:test_name`.
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
  once per run in a throwaway child; if no mode works, the runner warns once
  and runs without isolation, while a per-test setup failure after a successful
  probe fails the run loudly rather than silently running unisolated. All
  mounts happen in the forked child's private mount namespace via `pre_exec`,
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
- `src/diagnostics.rs` - On failure, parses `xtrace.log` to find the last
  executed command (skipping `++` subshell traces, so a failure inside
  `$(...)` is attributed to the statement around it), maps it back to the
  original source file, and renders an annotate-snippets error snippet.
  Also shows `left`/`right` operands for a failed `[`/`[[` comparison and,
  for `=`/`==`/`!=`, an inline character-level diff of them. The report the
  whole of this adds up to is documented in the README, so keep both in step
  when changing it.
  The log is scanned a line at a time, keeping at most `MAX_LINE` bytes of any
  one of them, rather than read whole: the failing command's own output sits
  between its trace line and the end of the file, so the trace cannot be found
  by reading a tail — but the output it has to be found behind is as big as the
  test made it, and need not contain a newline at all.
  The source file behind a snippet is read, and parsed for the function bounds
  that clamp the rendered window, once per file per run (`SourceCache`, owned
  by the run loop): the text and the bounds depend only on the file, while a
  failing suite reports one failure per *test*, so a file with n failing tests
  in it used to cost n reads and n shell parses on the parent's critical path.
  One read also keeps a snippet self-consistent — the annotated line and the
  bounds come from the same text rather than from two reads a test apart.
  The source lines the snippet renders go through `escape_c1` first:
  `annotate_snippets` substitutes the C0 controls in the source it is given
  but prints `U+0080`–`U+009F` through, and those are controls a UTF-8
  terminal obeys (`U+009B` is `CSI`). Escaping happens before the annotation
  span is measured, since it moves the offsets the span is built from.
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
  `xtrace`, `strace` and `resources`. The schema is documented in the README, so
  keep both in step when changing it.
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
no recognized shebang (`--shebang` overrides this for every test). Every command
in a test function is an implicit assertion - if it exits nonzero, the test
fails. Non-test functions (helpers/setup) are also extracted and made available
to tests.

Only function definitions are extracted (`build_functions_source` drops
everything else), so top-level code in a test file never runs: globals, `trap`s
and `set` options have to live inside a function. The generated runner script
enables `set -e` and `set -x` and nothing more — `pipefail` in particular is
off, so a test asserting on a pipeline must set it itself (see
`examples/race_condition.test`).

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
- `--no-overlay` — disable overlayfs isolation; run each test directly in the
  working directory (same as the automatic fallback when overlays are unavailable)
- `--repeat N` — run each test N times (default: 1); combine with `--fuzz` to
  shake out flaky tests
- `--fuzz [VALUE]` — randomly pause and resume each test's descendant processes
  (`SIGSTOP`/`SIGCONT`) to introduce timing non-determinism. Optional
  aggressiveness in (0,1); higher pauses more often (default: 0.5)
- `--shebang SHELL` — force this shell for every test, ignoring each script's own
  shebang (e.g. `/bin/bash`, `/usr/bin/zsh`)
- `--json` — print results as JSONL (one JSON object per test) instead of the
  colored terminal output
- `--no-cgroups` — disable cgroup resource tracking
- `-d`, `--debug` — enable debug logging

## Building and running

Use `cargo run --` to execute the project.

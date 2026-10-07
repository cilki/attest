## Architecture

- `src/main.rs` - CLI entry point using clap. Invoking `attest` with no
  subcommand runs the tests; `list` and `skill` are the only subcommands. The
  positional target is a file, a directory, or a `<file>/<test>` pattern, which
  is split into a path plus an implicit `--filter`. `parse_discovered` parses
  every discovered file: a parse error in a file the user named directly is
  fatal, while one in a file the directory walk turned up is reported on stderr
  and the file skipped, so an unparseable script elsewhere in the tree cannot
  abort the run.
- `src/discovery.rs` - Finds test files from a file path or directory
  (recursive). Scans all shell scripts (by extension or shebang) for test
  functions, not just `.test` files.
- `src/parser.rs` - Parses shell scripts with `brush_parser::Parser`, walks the
  AST to extract all `FunctionDefinition` nodes. Test functions start with
  `test`.
- `src/runner.rs` - For each test: writes all extracted functions (test +
  helper) to a temp script, forks a child that execs `/bin/sh -c`, redirects
  stdout to `stdout.log` and stderr to `xtrace.log`, enables `set -ex`, sources
  the script, then invokes the test function by name. The child pivots into a
  per-test ephemeral root (see `src/overlay.rs`) when available, runs in its own
  session (so timeouts and ^C kill the whole process tree via its process group
  and, when cgroups are active, `cgroup.kill`). Parallel by default via
  `fork(2)` with configurable parallelism (`--parallel`). Supports `--timeout`,
  `--bail`, `--override`, `--bin-dir`, and `--strace`. With `--save-context`,
  copies each test's filesystem delta plus logs to the output dir. Display
  names (and thus context dirs) are unique per test: a test name defined in
  more than one file is qualified as `file.test:test_name`.
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
- `src/diagnostics.rs` - On failure, parses `xtrace.log` to find the last
  executed command, maps it back to the original source file, and renders an
  annotate-snippets error snippet. Also shows inline character-level diffs for
  failed `[ A = B ]` assertions.
- `src/output.rs` - ANSI-colored terminal output for PASS/FAIL and summary.

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

## CLI options

- `--parallel N` — max concurrent tests (default: CPU cores)
- `--timeout SECS` — wall-clock timeout per test; timed-out tests show `TIME`
  and count as failures
- `--bail` — stop after first failure
- `--filter [FILE/]PATTERN` — run only matching tests (`*` wildcards, prefix
  match)
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
- `--no-cgroups` — disable cgroup resource tracking (only present when built with
  the `cgroup` feature)
- `-d`, `--debug` — enable debug logging

## Building and running

Use `cargo run --` to execute the project.

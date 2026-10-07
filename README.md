<p align="center">
	<img src="https://raw.githubusercontent.com/fossable/fossable/master/emblems/attest.svg" style="width:90%; height:auto;"/>
</p>

![License](https://img.shields.io/github/license/fossable/attest)
![GitHub repo size](https://img.shields.io/github/repo-size/fossable/attest)
![Stars](https://img.shields.io/github/stars/fossable/attest?style=social)

<hr>

![](./.github/assets/parallel.gif)

> perfection is finally attained not when there is no longer anything to add,
> but when there is no longer anything to take away
>
> Terre des Hommes (1939) - Antoine de Saint Exupéry

**attest** is a simple and modern test framework for CLI programs. There is no
exotic test syntax to remember, assertion API, plugins, or hidden lifecycle
methods to know about. Tests are just regular shell functions where every
statement is an assertion.

We already have all of the tools we need to write tests in the shell:

- Shell functions neatly organize tests into runnable units
- Need to compare text? `[` and `[[` have been around for decades
- Need to compare JSON? `jq -c` has you covered.
- Need some test setup/cleanup? Idiomatic with helper functions and traps.

By keeping the framework lightweight, tests are easy to write and quick to
understand, leading to an overall more effective testing experience.

## Writing tests

Here's an illustrative example of a test for the `md5sum` command:

```sh
## Test the md5sum command with known input/output
testHello() {
	result=$(echo hello | md5sum) # Test fails if nonzero exit

	[ "${result}" = "b1946ac92492d2347c6235b4d2611184  -" ] # Test fails if output changes
}
```

It looks like an ordinary shell script because it **is** an ordinary shell
script. You could even source it into your shell and run it directly if you
wanted to. Don't try that with Bats :).

There are only three implicit pieces of knowledge that you need for writing
tests:

- All test functions are named starting with `test`
- If any command in your function exits nonzero, the whole test fails
- Each test starts in its own clean, empty temporary working directory, and
  runs in its own copy-on-write view of the filesystem: it sees the real
  project files, but writes to the root filesystem, the project tree, `/tmp`,
  and `/var/tmp` are discarded after the run. (Other mounts — `/proc`, `/dev`,
  network mounts, etc. — stay shared with the host, so writes there persist.
  `--no-overlay` turns the copy-on-write view off; the fresh working directory
  stays.) Any processes still running when a test ends are killed
  automatically.

That working directory is a temporary one, not the directory you ran `attest`
from, so a relative path in a test is relative to the scratch directory and
reaches nothing in your project. Inside a test function `$0` is the absolute
path of the file the test came from, which is how you get back to the project:

```sh
testFixture() {
	[ "$(cat "$(dirname "$0")/fixture.txt")" = "expected" ]
}
```

Isolation is built out of overlayfs, which needs `CAP_SYS_ADMIN` — either
directly, or through a user namespace, in which case the test also sees itself
as `root` inside that namespace. attest rehearses the whole setup once per run
and, where neither route works, falls back to running tests without the
copy-on-write view, just like `--no-overlay` does: each test still gets its own
fresh working directory, but everything it writes outside that directory lands
on the real filesystem and stays there. Nested containers and sandboxes are the
usual places this happens.

### What attest actually runs

Those three facts are about the inside of a test function. There is one more
worth knowing about the file around them: attest never executes your test file.
It parses it, extracts the *function definitions* — tests and helpers alike —
and sources only those into a fresh shell, which then calls one test function.
Everything at the file's top level is dropped, which has two consequences:

- **Code outside a function never runs.** A variable assigned at the top of the
  file is unset inside the tests, and a top-level `trap` or `set` has no
  effect. Put shared values and setup in a helper function that each test
  calls.
- **Shell options belong inside the test function.** The runner turns on
  `set -e` (that is what makes every statement an assertion) and `set -x`, and
  nothing else — in particular `pipefail` is off, so `false | true` *passes*.
  To assert on a pipeline, enable it in the test itself, the way
  `examples/race_condition.test` does:

  ```sh
  testGrepQ() {
  	set -o pipefail

  	produce | consume
  }
  ```

Dropping the top level is also what makes inline tests work: sourcing a script
that happens to be a program doesn't run the program.

### Inline tests

If you're testing something that's itself a shell script, you can also include
your tests inline with the script.

<details>
<summary>For example:</summary>

```sh
#!/usr/bin/env bash

## Inline tests can be placed anywhere in the script. You can use $0 to call
## the script we're embedded in.

testGoodInput() {
	result=$($0 1 2)

	[ ${result} -eq 3 ]
}

testNoInput() {
	! $0
}

testBadInput() {
	! $0 1 1.2
}

testTooManyArgs() {
	! $0 1 2 3
}

# Here is the actual implementation of the script. It's not important for our
# purposes; I just prompted Claude for the most complicated way to add two
# numbers. The model calls it "enterprise grade" :)

python3 -c "
import subprocess, json, sys, os, tempfile, re

def validate(x):
  result = subprocess.run(
      ['bash', '-c', f'printf \"%d\" \"{x}\" 2>/dev/null || exit 1'],
      capture_output=True, text=True
  )
  if result.returncode != 0:
      raise ValueError(f'not an integer: {x}')
  return x

def add(a, b):
  validate(a)
  validate(b)

  with tempfile.NamedTemporaryFile(mode='w', suffix='.json', delete=False) as f:
      json.dump({'operands': [a, b], 'operation': 'addition'}, f)
      fname = f.name

  with open(fname) as f:
      payload = json.load(f)

  os.unlink(fname)

  result = subprocess.run(
      ['awk', '-v', f'a={payload[\"operands\"][0]}', '-v', f'b={payload[\"operands\"][1]}',
       'BEGIN { print a + b }'],
      capture_output=True, text=True
  )
  return result.stdout.strip()

if len(sys.argv) != 3:
  sys.exit(1)

try:
  print(add(sys.argv[1], sys.argv[2]))
except ValueError:
  sys.exit(1)
" "$1" "$2"
```

Inline tests can be nice because the closer the tests are to the code that's
being tested, the more likely they are maintained over time.

</details>

### Generating tests with AI

Tests are also easy for AIs to write. Use `attest skill` to print a skill which
is pre-tuned to produce good tests. Here's an example prompt:

> Write attest-style tests for the `tac` command.

<details>
<summary>Here's the output from Haiku 4.5:</summary>

```sh
#!/usr/bin/env bash

## Basic functionality - reverse lines from a file
testBasicFile() {
	echo -e "line1\nline2\nline3" > input.txt
	result=$(tac input.txt)
	[ "$result" = "$(echo -e "line3\nline2\nline1")" ]
}

## Reverse lines from stdin
testStdin() {
	result=$(echo -e "first\nsecond\nthird" | tac)
	[ "$result" = "$(echo -e "third\nsecond\nfirst")" ]
}

## Single line should remain unchanged
testSingleLine() {
	echo "only line" > input.txt
	result=$(tac input.txt)
	[ "$result" = "only line" ]
}

## Empty file produces no output
testEmptyFile() {
	touch empty.txt
	result=$(tac empty.txt)
	[ -z "$result" ]
}

## Multiple files are concatenated and reversed
testMultipleFiles() {
	echo -e "a\nb" > file1.txt
	echo -e "c\nd" > file2.txt
	result=$(tac file1.txt file2.txt)
	[ "$result" = "$(echo -e "d\nc\nb\na")" ]
}

## Non-existent file produces error
testNonExistentFile() {
	! tac nonexistent.txt 2>/dev/null
}

## Trailing newline handling
testTrailingNewline() {
	printf "line1\nline2\nline3" > input.txt
	result=$(tac input.txt)
	[ "$result" = "$(echo -e "line3\nline2\nline1")" ]
}

## Help flag works
testHelp() {
	tac --help || tac -h
}

## Version flag works
testVersion() {
	tac --version || tac -v
}

## Lines with special characters
testSpecialCharacters() {
	echo -e "hello\$world\nfoo@bar\n#comment" > input.txt
	result=$(tac input.txt)
	echo "$result" | grep -q "^#comment"
	echo "$result" | grep -q "foo@bar"
}

## File with only whitespace lines
testWhitespaceLines() {
	printf "line1\n  \nline3\n" > input.txt
	result=$(tac input.txt)
	[ "$(echo "$result" | wc -l)" -eq 3 ]
}

## Very long lines are handled correctly
testLongLines() {
	long_line=$(printf 'a%.0s' {1..1000})
	echo -e "$long_line\nshort" > input.txt
	result=$(tac input.txt)
	echo "$result" | grep -q "^short$"
}

## Reads from multiple sources correctly
testMixedInput() {
	echo "file line" > file.txt
	echo "stdin line" | tac - file.txt | head -1 | grep -q "file line"
}
```

</details>

AI can generate tests all day, so the important thing is how easy it is for a
human to quickly understand and assess the quality of AI-produced tests.

## Running tests

![](./.github/assets/serial.gif)

Now that we have some tests, AI-generated or not, it's time for the good part.

```sh
# Just run the tests in one file
attest example.test

# Run all tests in this directory
attest .

# Tests run in parallel by default; use --parallel to limit concurrency
attest --parallel 1 .

# List the tests that would run, without running them
attest list .
```

Pointed at a directory, `attest` walks it recursively and scans every shell
script it finds for test functions: files named `*.test`, `*.sh` or `*.bash`,
plus any other file whose first line is a shebang naming a shell (`sh`, `bash`,
`zsh`, `dash`, `ash`, `ksh`). Hidden files and directories are skipped, and so
are symlinks: `attest` runs every test function it finds, so a `result` link
into the nix store or a `tests/fixtures` link to a sibling checkout would
otherwise mean running code from a directory you never pointed it at. Anything
that isn't a regular file is skipped too: a named pipe sitting in the tree would
block the walk waiting for a writer. Pointed at a single file — or a symlink to
one — it scans that whatever it happens to be called.

Not every script in a tree is shell `attest` can parse — zsh-only syntax, a
generated or templated file, or just a script with a syntax error in it. Those
are reported on stderr and skipped, so one unrelated file can't take down a run
it has no tests in. Point `attest` straight at such a file, though, and the
parse error is fatal: you asked for that file specifically.

Each test runs under the shell its file's shebang asks for, falling back to
`/bin/sh` when that shell isn't installed and to `bash` for files without a
recognized shebang.

By default you get a progress bar while the run is in flight, a summary at the
end, and a report for each failure. Add `-v` if you also want a PASS/FAIL line
per test.

Control characters in a test or file name are escaped (`\x1b`, `\x0d`, and the
C1 forms such as `\x9b`, which a terminal in UTF-8 mode obeys just the same)
wherever that name appears in the report. A shell takes almost anything as a
function name, including bytes a terminal reads as cursor movement — and the
name sits right next to the `PASS`/`FAIL` the report is for, so a test could
otherwise erase its own verdict and write a nicer one. `--json` is unaffected:
nothing interprets a name there, so it comes back byte for byte.

Every test runs in a temporary _context directory_ that collects logs and
temporary files created by the test.

### Selecting tests

Any `<file>/<test>` pair works as a target, and `--filter` narrows a wider run
down the same way. A name without a `*` matches as a prefix, so you rarely have
to spell one out in full:

```sh
# Just one test
attest examples/md5sum.test/testHello

# Every test in one file, wherever that file turns up under the given directory
attest . --filter 'md5sum.test/'

# Every test whose name starts with "testHel"
attest . --filter testHel

# `*` is a wildcard
attest . --filter 'testVer*'
```

A filter is `[<file>/]<name>`, and the `/` is what tells the two halves apart:

- **With no `/` the whole thing is a test name**, never a file — which is why
  the file-only example above ends in one. `--filter md5sum.test` looks for a
  test whose name starts with `md5sum.test` and so matches nothing. The one
  exception is a leading `./`, which marks the argument as a path: all of it is
  then the file half, so `--filter ./md5sum.test` and
  `--filter ./examples/md5sum.test` mean the same as `'md5sum.test/'` and
  `'examples/md5sum.test/'`. Nothing is left for a name, so one cannot be
  appended behind the marker: as a *filter*,
  `./examples/md5sum.test/testHello` asks for a file at that whole path. Spell
  a filter that carries a name without the `./`, the way `list` prints it. As a
  *target* the same string still works — there the split is decided by the
  filesystem, and only where the left half is a file that exists.
- **The file half is a path suffix**, compared a whole component at a time and
  with no wildcards: `md5sum.test/` and `examples/md5sum.test/` both select the
  file, while `um.test/` and `md5*.test/` select nothing. An empty name half
  (`md5sum.test/`) means every test in the file.
- **The name half is where `*` works.** Without one it matches as a prefix
  (`testHel` catches `testHelp` and `testHello`); with one it has to match the
  name in full, so `*Help` is a suffix match and `test*` is everything.

`list` accepts the same targets and `--filter`, so you can check what a
selection covers before running it. What it prints is a selector of exactly the
same shape, relative to the directory you ran it in, so a line of its output
goes straight back as a target or a filter:

```sh
❯ attest list examples/md5sum.test
examples/md5sum.test/testHelp
examples/md5sum.test/testVersion
examples/md5sum.test/testHello

❯ attest examples/md5sum.test/testHello
```

The directories are part of the selector because the file half is only a
suffix: two files named `x.test` in different directories would otherwise list
identically, and either line would select both of their tests.

### Shell completion

`attest` completes targets for you: the script files it would discover, and the
test functions inside them. There's no separate completion file to install —
the binary prints its own completion script when `COMPLETE` names your shell:

```sh
# bash (~/.bashrc)
source <(COMPLETE=bash attest)

# zsh (~/.zshrc, after compinit)
source <(COMPLETE=zsh attest)

# fish (~/.config/fish/config.fish)
COMPLETE=fish attest | source
```

With that in place, `attest examples/md5<TAB>` completes the file name and
`attest examples/md5sum.test/<TAB>` offers `testHelp`, `testVersion` and
`testHello`. `--filter` completes the same way.

### Other options

- `--timeout SECS` — kill a test after this much wall-clock time and report it
  as `TIME`
- `--bail` — end the run at the first failure. The tests still in flight are
  abandoned rather than killed, so the run itself lasts as long as the slowest
  of them (see [Stopping a run early](#stopping-a-run-early))
- `--repeat N` — run each test N times
- `--json` — print one JSON object per test instead of the colored output
  (schema below)
- `--override SPEC` — copy a binary into the test's `bin/` dir so tests resolve
  that name to it. `SPEC` is a path (`/usr/bin/example`) or a mapping
  (`example=/usr/bin/override`)
- `--bin-dir DIR` — prepend DIR to each test's PATH, without copying anything
- `--strace CMD` — run CMD under strace, saving the log to the test's context
  dir. `CMD` is a command name or a path to one; tests that call it by its base
  name get the traced version
- `--shebang SHELL` — force one shell for every test, ignoring each file's own
  shebang
- `--no-overlay` — skip the copy-on-write view of the filesystem, so writes
  outside the test's own working directory are kept instead of discarded
- `--no-cgroups` — don't track per-test CPU, memory and IO usage with cgroups
- `-d`, `--debug` — enable debug logging

### Stopping a run early

Normally a run ends once every selected test has finished, and the summary
counts all of them. Two things cut a run short, and they do it differently —
one abandons the tests still in flight, the other kills them:

- **`--bail`.** The first failure ends the run. No further tests are launched,
  _and the tests that were already running are abandoned_ — they are never
  reported, as neither failures nor skips, so the summary's total counts only
  the tests that had finished by then. With `--parallel 4`, a failure in the
  very first batch can leave a hundred-test selection reporting `1 total`:

  ```
  ❯ attest . --bail --parallel 4
  FAIL  testAFastFail                            (5ms)
  ...

  Results: 0 passed, 1 failed, 1 total
  Time:   4.06s
  ```

  Abandoned is not the same as killed, though, and the `4.06s` is where that
  shows: the three tests sharing the first batch with the failure were each a
  `sleep 4`, and `attest` waited for every one of them to exit on its own
  before giving up on it. So `--bail` makes a run _fail_ fast, not _finish_
  fast. Everything those tests produced is still discarded — no report line, no
  `--save-context` directory, nothing in the total — but they do run to the
  end, traps and side effects included, and nothing bounds the wait: a single
  `sleep 600` in flight holds the run for ten minutes after the failure has
  been printed. Pair `--bail` with `--timeout`, which does kill and so caps the
  wait at one timeout, or with `--parallel 1`, where there is nothing in flight
  to wait for.

  The exit status is 1, as for any run with a failure in it.

- **`^C` (or `SIGTERM`).** This one kills: every running test's process tree is
  `SIGKILL`ed, so no `trap` inside a test runs on the way out, and `attest`
  abandons the run. There is no summary at all, just
  `Error: interrupted; killed 2 running test(s)` on stderr and exit status 1.
  Results from tests that already finished are discarded along with it, so use
  `-v` (or `--json`) if you want a record of them as they go. Tests run in
  their own sessions, which is why `attest` has to do this killing itself: the
  terminal's `^C` reaches `attest` only, never the tests.

A run also winds down early if whatever was reading its output goes away:
`attest . | head -1` kills the running tests and dies of `SIGPIPE` like any
other filter, rather than finishing the suite with nobody left to report to.

### JSON output

`--json` replaces the colored output with JSONL: one object per test, printed
as each test finishes, so another program can consume the results.

```sh
attest . --json | jq -c '{name, status, duration_ms}'
```

Every object carries the same keys:

- `name` — display name of the test, verbatim (the terminal output escapes
  control characters in it, this does not)
- `file` — absolute path of the script the test was found in
- `status` — `pass`, `fail`, or `timeout` (the `--timeout` was exceeded)
- `duration_ms` — wall-clock time of the test, in milliseconds
- `stdout` — everything the test wrote to stdout
- `xtrace` — the test's xtrace log (its stderr, with the traced commands)
- `strace` — object mapping each `--strace` command to its log, keyed by the
  command's base name (`--strace /usr/bin/curl` appears as `curl`); `{}`
  without `--strace`
- `resources` — cgroup usage, or `null` when it isn't available

When present, `resources` holds whichever of `cpu_user_usec`,
`cpu_system_usec`, `memory_peak`, `io_read_bytes`, `io_write_bytes` and
`pids_peak` the cgroup reported — the same numbers as the resource usage line
described below, in microseconds and bytes.

The exit status is unchanged by `--json`: 1 if any test failed, 0 otherwise.

### Containerized tests

If your application requires some dependencies in a Docker container, you can
run `attest` in a container with this recipe:

```sh
docker run --rm -v $(which attest):/bin/attest -v $(pwd):/tests <image name> attest /tests
```

Containers usually can't mount overlayfs, so tests run this way tend to land on
the unisolated fallback described above: a test that writes to a path under
`/tests` changes your real files. Scratch writes are unaffected — the working
directory each test starts in is temporary either way.

### Fuzz testing

If your application spawns subprocesses, `attest` can randomly distort their
timing:

```sh
attest --fuzz examples/race_condition.test
```

While a test runs, `attest` keeps picking one of its descendant processes at
random and either pausing it with `SIGSTOP` or letting a previously paused one
go again with `SIGCONT`. A subprocess therefore stays stopped for an
unpredictable while rather than a fixed delay. This option also works nicely
with `--repeat`.

<details>
<summary>Example</summary>

Without `--fuzz`, you might not realize there's a nasty race condition hiding in
this file (`-v` gives us a line per test instead of just the summary):

```
❯ attest -v --parallel 1 --repeat 10 examples/race_condition.test
PASS  testGrepQ#1                              (1.06s)
      cpu=7.8ms+4.8ms  mem=2.8MiB  pids=5
PASS  testGrepQ#2                              (1.07s)
      cpu=6.2ms+6.2ms  mem=3.1MiB  pids=5
PASS  testGrepQ#3                              (1.07s)
      cpu=6.2ms+6.2ms  mem=2.6MiB  pids=5
PASS  testGrepQ#4                              (1.07s)
      cpu=6.2ms+6.2ms  mem=3.4MiB  pids=5
PASS  testGrepQ#5                              (1.07s)
      cpu=6.3ms+6.3ms  mem=3.4MiB  pids=5
PASS  testGrepQ#6                              (1.06s)
      cpu=8.7ms+3.7ms  mem=3.4MiB  pids=5
PASS  testGrepQ#7                              (1.07s)
      cpu=5.9ms+6.9ms  mem=3.0MiB  pids=5
PASS  testGrepQ#8                              (1.07s)
      cpu=6.3ms+6.3ms  mem=3.1MiB  pids=5
PASS  testGrepQ#9                              (1.06s)
      cpu=6.1ms+6.1ms  mem=3.1MiB  pids=5
PASS  testGrepQ#10                             (1.07s)
      cpu=6.2ms+6.2ms  mem=2.8MiB  pids=5

Results: 10 passed, 10 total
Time:   10.69s
```

Now let's add some fuzziness to the timing (each `FAIL` is also followed by the
test's xtrace output and a diagnostic snippet pointing at the failing command,
elided here):

```
❯ attest -v --parallel 1 --fuzz 0.9 --repeat 10 examples/race_condition.test
PASS  testGrepQ#1                              (3.47s)
      cpu=5.5ms+7.3ms  mem=2.8MiB  pids=5
FAIL  testGrepQ#2                              (4.09s)
      cpu=6.7ms+6.0ms  mem=3.2MiB  pids=5
FAIL  testGrepQ#3                              (5.10s)
      cpu=4.7ms+7.8ms  mem=2.9MiB  pids=5
PASS  testGrepQ#4                              (3.59s)
      cpu=6.3ms+6.3ms  mem=3.5MiB  pids=5
FAIL  testGrepQ#5                              (2.39s)
      cpu=5.8ms+6.8ms  mem=3.1MiB  pids=5
PASS  testGrepQ#6                              (6.51s)
      cpu=5.3ms+7.4ms  mem=3.1MiB  pids=5
FAIL  testGrepQ#7                              (3.08s)
      cpu=7.1ms+5.7ms  mem=3.1MiB  pids=5
FAIL  testGrepQ#8                              (3.34s)
      cpu=4.3ms+8.1ms  mem=3.4MiB  pids=5
PASS  testGrepQ#9                              (2.68s)
      cpu=6.3ms+6.3ms  mem=3.1MiB  pids=5
FAIL  testGrepQ#10                             (2.08s)
      cpu=6.2ms+6.2ms  mem=2.6MiB  pids=5

Results: 4 passed, 6 failed, 10 total
Time:   36.35s
```

We were able to shake out the race condition by adding random delays in the
test. The `grep -q` example above is obviously contrived, but imagine you were
checking for firewall rules with `iptables | grep -q`.

You'll also notice the test took over 3 times longer. You can adjust how
aggressive the fuzzer is with `--fuzz`'s optional value, which must be strictly
between 0 and 1 (default `0.5`): the higher it is, the more often a process is
paused instead of resumed.

</details>

## Debugging tests

![](./.github/assets/diagnostic.gif)

When a test fails, you can save its context:

```sh
attest . --save-context ./results
```

This directory contains, per test, its xtrace and stdout logs, the test's
working directory under `cwd/` (so a scratch file written to `$PWD` shows up at
`results/<test>/cwd/x`), and every file it created or modified elsewhere, laid
out by absolute path (a write to `/tmp/x` shows up at
`results/<test>/tmp/x`).

That last part is the overlay's upper layer, so it only appears when the
copy-on-write view is in use. Under `--no-overlay` — or the fallback attest
takes when overlayfs is unavailable — writes outside the working directory went
straight to the real filesystem and are still there, so the saved context is
just the logs and `cwd/`.

Saved files keep their permissions, minus the set-user-ID, set-group-ID and
sticky bits: those files are whatever the test chose to write, and the copy is
owned by whoever ran attest (root, when it runs under `sudo` so that overlays
can be mounted), so a test that left a `chmod u+s` binary behind must not get
one carried out of its sandbox for anyone with access to the saved context to
run.

To see the syscalls a command makes, trace it with `--strace` and save the
context — the log lands at `results/<test>/strace/<cmd>.log`:

```sh
attest . --strace curl --save-context ./results
```

Failed tests always print their xtrace output. You can also stream the xtrace
output live with the `-vv` flag:

![](./.github/assets/xtrace.gif)

### Reading a failure report

A failure is reported in three parts: the `FAIL` line, the test's xtrace log,
and a snippet of the source. Here's the whole thing for the `md5sum` test from
the top of this README, with one digit of the expected hash changed:

```
❯ attest md5sum.test
FAIL  testHello                                (50ms)
--- xtrace: testHello ---
+8: testHello
++5: md5sum
++5: echo hello
+5: result='b1946ac92492d2347c6235b4d2611184  -'
+7: '[' 'b1946ac92492d2347c6235b4d2611184  -' = 'b1946ac92492d2347c6235b4d2611185  -' ']'
error: command failed
 --> md5sum.test:7:2
  |
4 | testHello() {
5 |     result=$(echo hello | md5sum)
6 |
7 |     [ "${result}" = "b1946ac92492d2347c6235b4d2611185  -" ]
  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
8 | }
  |

  left: "b1946ac92492d2347c6235b4d2611184  -"
 right: "b1946ac92492d2347c6235b4d2611185  -"
  diff: "b1946ac92492d2347c6235b4d2611184  -"
        "b1946ac92492d2347c6235b4d2611185  -"
```

Each trace line is prefixed with the line of your file the command came from,
so `+7:` is the `[` on line 7. A `++` prefix marks a command that ran in a
subshell — here the two halves of `$(echo hello | md5sum)`. The exception is
the very first line, which is the runner calling the test function: its number
belongs to the small script `attest` generates, not to your file.

The snippet points at the last command the trace recorded, with up to three
lines of context on either side, clamped to the enclosing function so it never
runs on into the next test. Subshell lines are skipped when locating it, so a
command that failed inside `$(...)` or in one side of a pipe is reported at the
statement containing it rather than at the command itself. A timed-out test
gets the same snippet under `test timed out`, pointing at whatever it was
running when the clock ran out.

When the failing command is a comparison — `[` or `[[` with `=`, `==`, `!=`,
`-eq`, `-ne`, `-lt`, `-le`, `-gt` or `-ge` — both operands are recovered from
the trace and printed as `left` and `right`, so you don't have to pick them out
of the quoting yourself. For `=`, `==` and `!=` a character-level `diff` of the
two follows, which is what finds the stray space or the changed digit: the
characters that differ are colored (red on the first line, green on the
second), so in the plain text above the `diff` lines look just like the `left`
and `right` ones. Newlines and tabs in an operand are printed as `\n` and `\t`
to keep all four lines aligned.

### Where a test's own output goes

A test never writes to your terminal. Both of its streams are captured into its
context directory:

- stdout goes to `stdout.log`
- stderr goes to `xtrace.log`, interleaved with the trace, so each line lands
  directly under the command that produced it

Only `xtrace.log` is ever printed — it's what you see under a `FAIL` and what
`-vv` streams live. A test's stdout is invisible unless you keep the context with
`--save-context` (or read it out of `--json`), which is worth knowing before you
reach for `echo` to debug a failure:

```sh
testFails() {
	echo "where did this go?"    # stdout.log, which nothing prints
	echo "this one shows up" >&2 # the FAIL report, right below its own command
	false
}
```

```
❯ attest demo.test
FAIL  testFails                                (50ms)
--- xtrace: testFails ---
+8: testFails
+4: echo 'where did this go?'
+5: echo 'this one shows up'
this one shows up
+6: false
...
```

So send anything you want to see on failure to stderr, and don't redirect either
stream to `/dev/null`: that throws away the only report you get.

Control bytes in a printed log are escaped (`\x1b`, `\x0d`, and the C1 controls
spelled out in UTF-8 as `\xc2\x9b`, …), keeping only newlines and tabs: a log is
replayed into the terminal reporting on the test, and a cursor-movement sequence
in it would otherwise erase and rewrite the lines around it. `--json` and
`--save-context` still give you the log byte for byte.

A test's context directory is live — that's where its logs are written and where
its working directory sits — so a test can also delete `stdout.log` or
`xtrace.log` and put something else at the name. `attest` reads back only the
regular file it opened there itself: a symlink left in a log's place is not
resolved to whatever host file it names, and a named pipe is not waited on for a
writer that will never come. Either way the log simply reads as missing.

Standard input, by contrast, is _not_ redirected — every test inherits
`attest`'s own stdin, and all of them share it. A command inside a test that
reads stdin therefore consumes whatever `attest` was given, two tests reading it
in parallel race over the same bytes, and if nothing is ever written there (an
idle terminal, say) the test simply blocks until `--timeout` kills it — or for as
long as the run lasts, without one. Redirect explicitly whenever a test runs
something that might read stdin: `mycmd < input.txt`, or `mycmd < /dev/null`.

### Resource usage

Result lines can carry a second line describing what the test actually
consumed:

```
PASS  testGrepQ#1                              (1.06s)
      cpu=7.8ms+4.8ms  mem=2.8MiB  pids=5
```

`cpu` is user plus system time, `mem` the peak memory, `io` the bytes read and
written, and `pids` the largest number of processes alive at once. Fields with
nothing to report are left out.

These numbers come from a cgroup created per test, so the line is absent
altogether when `attest` can't make one — cgroup v2 not mounted, or no
permission to create a child cgroup, which `-d` will tell you about — and when
you pass `--no-cgroups`.

## Installation

`attest` runs on Linux only: test isolation is built out of overlayfs and
`pivot_root`, and resource usage comes from cgroup v2, none of which have
equivalents elsewhere. It does not compile on macOS or Windows.

<details>
<summary>Crates.io</summary>

![Crates.io Total Downloads](https://img.shields.io/crates/d/attest)

#### Install from crates.io

```sh
cargo install attest
```

</details>

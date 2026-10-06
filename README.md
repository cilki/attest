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
  `--no-overlay` disables isolation entirely.) Any processes still running
  when a test ends are killed automatically.

Isolation is built out of overlayfs, which needs `CAP_SYS_ADMIN` — either
directly, or through a user namespace, in which case the test also sees itself
as `root` inside that namespace. attest rehearses the whole setup once per run
and, where neither route works, falls back to running every test directly in
the working directory just like `--no-overlay` does, so writes are no longer
discarded. Nested containers and sandboxes are the usual places this happens.

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
`zsh`, `dash`, `ash`, `ksh`). Hidden files and directories are skipped. Pointed
at a single file, it scans that file whatever it happens to be called.

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

`list` accepts the same targets and `--filter`, so you can check what a
selection covers before running it.

### Other options

- `--timeout SECS` — kill a test after this much wall-clock time and report it
  as `TIME`
- `--bail` — stop launching new tests after the first failure
- `--repeat N` — run each test N times
- `--json` — print one JSON object per test instead of the colored output
- `--override SPEC` — copy a binary into the test's `bin/` dir so tests resolve
  that name to it. `SPEC` is a path (`/usr/bin/example`) or a mapping
  (`example=/usr/bin/override`)
- `--bin-dir DIR` — prepend DIR to each test's PATH, without copying anything
- `--strace CMD` — run CMD under strace, saving the log to the test's context
  dir. `CMD` is a command name or a path to one; tests that call it by its base
  name get the traced version
- `--shebang SHELL` — force one shell for every test, ignoring each file's own
  shebang
- `--no-overlay` — skip filesystem isolation and run each test directly in the
  working directory
- `--no-cgroups` — don't track per-test CPU, memory and IO usage with cgroups
- `-d`, `--debug` — enable debug logging

### Containerized tests

If your application requires some dependencies in a Docker container, you can
run `attest` in a container with this recipe:

```sh
docker run --rm -v $(which attest):/bin/attest -v $(pwd):/tests <image name> attest /tests
```

Containers usually can't mount overlayfs, so tests run this way tend to land on
the unisolated fallback described above — writes to the mounted project tree
reach your real files.

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

To see the syscalls a command makes, trace it with `--strace` and save the
context — the log lands at `results/<test>/strace/<cmd>.log`:

```sh
attest . --strace curl --save-context ./results
```

Failed tests always print their xtrace output. You can also stream the xtrace
output live with the `-vv` flag:

![](./.github/assets/xtrace.gif)

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

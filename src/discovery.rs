use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// Shell-related file extensions that are always scanned for test functions.
const SHELL_EXTENSIONS: &[&str] = &["test", "sh", "bash"];

pub fn discover_test_files(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }

    if path.is_dir() {
        let mut files = Vec::new();
        collect_script_files(path, &mut files)?;
        files.sort();
        if files.is_empty() {
            bail!("no script files found in {}", path.display());
        }
        return Ok(files);
    }

    bail!("path does not exist: {}", path.display());
}

/// Recursively collect the shell scripts under `dir`, skipping hidden entries
/// and never following symlinks.
///
/// Following them would make the set of tests a run covers unbounded: the walk
/// descends into whatever a symlinked directory names, and `attest` *executes*
/// every test function it finds. A `result` link into the nix store, a
/// `node_modules` link to a sibling checkout or a stray `tests/x -> /` all turn
/// "run the tests in this directory" into running code from somewhere the
/// caller never named. A link pointing back at an ancestor is worse still:
/// `ln -s . loop` makes the same file turn up once per level until path
/// resolution gives up at the kernel's symlink limit, so every test in the tree
/// runs forty times over.
///
/// Only regular files are considered for scanning, for the same reason: a
/// non-regular file the walk merely stumbled on is not something to open. A
/// named pipe is the sharp case — [`is_shell_script`] reads the first line of
/// anything without a known extension, and opening a FIFO blocks until a writer
/// shows up, so a single `mkfifo`'d file anywhere in the tree used to hang the
/// whole run before it reported a thing.
///
/// A path passed to `attest` directly is still scanned whatever it is — the
/// caller named it, so there is nothing unbounded about it (see
/// [`discover_test_files`]).
fn collect_script_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        // On the bytes, so a name that is not UTF-8 is still hidden when it
        // starts with a dot, rather than falling through to the scan.
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let path = entry.path();
        // `file_type()` comes from the directory entry (or an lstat), so a
        // symlink reports as one rather than as whatever it points at.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_script_files(&path, files)?;
        } else if file_type.is_file() && is_shell_script(&path) {
            files.push(path);
        }
    }
    Ok(())
}

/// A file is considered a shell script if it has a known shell extension or a
/// shell shebang on its first line.
fn is_shell_script(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && SHELL_EXTENSIONS.contains(&ext)
    {
        return true;
    }

    // Only the first line of the head is decoded. The read is a fixed length
    // and may well stop inside a multibyte character further down; decoding
    // the whole head used to fail on that and lose the shebang in front of
    // it, so an extension-less script with non-ASCII text in its first few
    // lines was not discovered.
    let mut buf = [0u8; 256];
    let Ok(n) = std::fs::File::open(path).and_then(|mut file| file.read(&mut buf)) else {
        return false;
    };
    let head = &buf[..n];
    let line = &head[..memchr::memchr(b'\n', head).unwrap_or(n)];
    std::str::from_utf8(line).is_ok_and(|line| shebang_shell(line).is_some())
}

/// The shell a shebang line asks for — `#!/bin/bash` and `#!/usr/bin/env bash`
/// alike — or `None` when it names no shell we recognize, or is not a shebang
/// at all.
///
/// The one place that answers both questions asked of a shebang — whether the
/// file is a shell script and which shell it wants — so the two can never
/// disagree about an interpreter.
fn shebang_shell(line: &str) -> Option<&str> {
    let mut parts = line.strip_prefix("#!")?.split_whitespace();
    let first = parts.next()?;
    let interpreter = if first.ends_with("/env") {
        parts.next()?
    } else {
        first
    };
    let basename = interpreter
        .rsplit_once('/')
        .map_or(interpreter, |(_, base)| base);
    matches!(basename, "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh").then_some(interpreter)
}

/// The path `shell` execs as: itself when it contains a `/` and exists, its
/// match on `PATH` when it is a bare name, else `None`.
fn shell_path(shell: &str) -> Option<String> {
    if shell.contains('/') {
        Path::new(shell).exists().then(|| shell.to_string())
    } else {
        which::which(shell)
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
    }
}

/// Resolve a shell name or path to an executable path, falling back to
/// `/bin/sh` when the requested shell is not found.
///
/// The answer is always a path, never a bare name: the runner execs it with
/// `execv` (see [`crate::spawn`]), which does no `PATH` search, so a bare
/// `bash` — what `#!/usr/bin/env bash` and a file with no shebang ask for —
/// would be looked up relative to the working directory and fail with
/// `ENOENT`.
///
/// Answered once per distinct shell name and remembered. Every file the walk
/// turns up asks this question, almost always about the same handful of
/// shells, and answering it is not free: a bare name goes to
/// [`which::which`], which stats the name in every entry of `PATH` — around
/// 20µs a file here, and the dominant cost of walking a tree of scripts that
/// hold no tests. Caching it also keeps one run's answer consistent, the way
/// settling a file's shell at parse time keeps one file's answer consistent:
/// a shell appearing on `PATH` mid-run cannot make two files that name the
/// same shell disagree about it.
///
/// The lookup runs under the lock, so two parse workers asking about the same
/// shell at once do not both pay for it — and cannot get two answers.
pub(crate) fn resolve_shell(shell: &str) -> String {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex, PoisonError};

    static RESOLVED: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);
    // A poisoned lock would mean a panic while resolving, which leaves the map
    // itself intact: take it back rather than turning that into a second panic.
    RESOLVED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(shell.to_owned())
        .or_insert_with(|| shell_path(shell).unwrap_or_else(|| "/bin/sh".to_owned()))
        .clone()
}

/// The shell to run the script `source` under: the one its shebang names if
/// that shell is installed, `/bin/sh` when it is not, and `bash` when the file
/// carries no shebang this recognizes.
///
/// Derived from the script's *text*, not from its path. By the time the answer
/// is needed the file has already been read whole and parsed
/// ([`crate::parser::parse_test_file`]), so taking the path would mean
/// reopening and re-reading it — which the runner used to do once per spawned
/// test, and so once more for every `--repeat`. Reading the shebang from the
/// same text the sourced script is cut from also keeps the two in step: a file
/// edited mid-run could otherwise have its new shell paired with its old
/// functions.
pub(crate) fn script_shell(source: &str) -> String {
    let named = source
        .lines()
        .next()
        .and_then(shebang_shell)
        .unwrap_or("bash");
    resolve_shell(named)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn discover_single_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("example.sh");
        fs::write(&file, "#!/bin/bash\necho hello\n").unwrap();

        let result = discover_test_files(&file).unwrap();
        assert_eq!(result, vec![file]);
    }

    #[test]
    fn discover_directory_finds_shell_files_in_order() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("z.sh"), "#!/bin/bash\n").unwrap();
        fs::write(tmp.path().join("c.txt"), "not a script\n").unwrap();
        fs::write(tmp.path().join("b.test"), "#!/bin/bash\n").unwrap();
        fs::write(tmp.path().join("a.sh"), "#!/bin/bash\n").unwrap();

        let result = discover_test_files(tmp.path()).unwrap();
        let expected: Vec<_> = ["a.sh", "b.test", "z.sh"]
            .iter()
            .map(|name| tmp.path().join(name))
            .collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn walk_skips_hidden_entries() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.test"), "#!/bin/sh\n").unwrap();
        fs::write(tmp.path().join(".hidden.test"), "#!/bin/sh\n").unwrap();
        // A hidden name that is not UTF-8 is hidden all the same.
        fs::write(
            tmp.path().join(OsStr::from_bytes(b".\xff.test")),
            "#!/bin/sh\n",
        )
        .unwrap();
        fs::create_dir(tmp.path().join(".dir")).unwrap();
        fs::write(tmp.path().join(".dir/b.test"), "#!/bin/sh\n").unwrap();

        let result = discover_test_files(tmp.path()).unwrap();
        assert_eq!(result, vec![tmp.path().join("a.test")]);
    }

    #[test]
    fn discover_directory_recursive() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(tmp.path().join("top.sh"), "#!/bin/bash\n").unwrap();
        fs::write(sub.join("nested.bash"), "#!/bin/bash\n").unwrap();

        let result = discover_test_files(tmp.path()).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn walk_does_not_descend_into_symlinked_directories() {
        // A link to a directory outside the tree would make `attest <dir>` run
        // test functions from files the caller never pointed at.
        let tmp = TempDir::new().unwrap();
        let tree = tmp.path().join("tree");
        let outside = tmp.path().join("outside");
        fs::create_dir(&tree).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(tree.join("inside.test"), "#!/bin/sh\n").unwrap();
        fs::write(outside.join("outside.test"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();

        let result = discover_test_files(&tree).unwrap();
        assert_eq!(result, vec![tree.join("inside.test")]);
    }

    #[test]
    fn walk_does_not_follow_symlinked_files() {
        let tmp = TempDir::new().unwrap();
        let tree = tmp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        let real = tmp.path().join("elsewhere.test");
        fs::write(&real, "#!/bin/sh\n").unwrap();
        fs::write(tree.join("inside.test"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&real, tree.join("linked.test")).unwrap();

        let result = discover_test_files(&tree).unwrap();
        assert_eq!(result, vec![tree.join("inside.test")]);
    }

    #[test]
    fn walk_terminates_on_a_symlink_loop() {
        // `ln -s . loop` resolves until the kernel's symlink limit, so a walk
        // that followed it yielded the same file once per level.
        let tmp = TempDir::new().unwrap();
        let tree = tmp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("a.test"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(".", tree.join("loop")).unwrap();

        let result = discover_test_files(&tree).unwrap();
        assert_eq!(result, vec![tree.join("a.test")]);
    }

    #[test]
    fn a_directly_named_symlink_is_still_scanned() {
        // Only the walk refuses to follow links; a path the caller named is
        // exactly what they asked for, link or not.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real.test");
        fs::write(&real, "#!/bin/sh\n").unwrap();

        let link = tmp.path().join("link.test");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(discover_test_files(&link).unwrap(), vec![link]);

        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("in.test"), "#!/bin/sh\n").unwrap();
        let dir_link = tmp.path().join("dir_link");
        std::os::unix::fs::symlink(&dir, &dir_link).unwrap();
        assert_eq!(
            discover_test_files(&dir_link).unwrap(),
            vec![dir_link.join("in.test")]
        );
    }

    #[test]
    fn walk_does_not_open_a_named_pipe() {
        // Reading the first line of a FIFO blocks until somebody writes to it,
        // so the walk must not treat one as a candidate script. Run it off the
        // test thread so a regression fails here instead of hanging the suite.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.test"), "#!/bin/sh\n").unwrap();
        use std::os::unix::ffi::OsStringExt;
        let fifo = std::ffi::CString::new(tmp.path().join("pipe").into_os_string().into_vec())
            .expect("fifo path has no NUL");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);

        let dir = tmp.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(discover_test_files(&dir)));
        let found = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the walk blocked on the FIFO")
            .unwrap();
        assert_eq!(found, vec![tmp.path().join("a.test")]);
    }

    #[test]
    fn discover_empty_directory_errors() {
        let tmp = TempDir::new().unwrap();
        let result = discover_test_files(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn discover_nonexistent_path_errors() {
        let result = discover_test_files(Path::new("/nonexistent/path/xyz"));
        assert!(result.is_err());
    }

    #[test]
    fn shell_script_by_extension() {
        let tmp = TempDir::new().unwrap();
        for ext in &["sh", "bash", "test"] {
            let file = tmp.path().join(format!("file.{ext}"));
            fs::write(&file, "no shebang\n").unwrap();
            assert!(is_shell_script(&file), "expected {ext} to be recognized");
        }
    }

    #[test]
    fn shell_script_detected_by_shebang() {
        let tmp = TempDir::new().unwrap();

        let bash_file = tmp.path().join("direct");
        fs::write(&bash_file, "#!/bin/bash\necho hi\n").unwrap();
        assert!(is_shell_script(&bash_file));

        let env_file = tmp.path().join("env_style");
        fs::write(&env_file, "#!/usr/bin/env bash\necho hi\n").unwrap();
        assert!(is_shell_script(&env_file));

        let python_file = tmp.path().join("not_shell");
        fs::write(&python_file, "#!/usr/bin/python3\nprint('hi')\n").unwrap();
        assert!(!is_shell_script(&python_file));

        let no_newline = tmp.path().join("one_line");
        fs::write(&no_newline, "#!/bin/sh").unwrap();
        assert!(is_shell_script(&no_newline));

        // The head is read as a fixed 256 bytes, which here ends in the middle
        // of a multibyte character: the shebang in front of it still counts.
        let cut = tmp.path().join("cut_mid_char");
        let mut text = b"#!/bin/sh\n# ".to_vec();
        while text.len() < 255 {
            text.push(b'x');
        }
        text.extend_from_slice("é\n".as_bytes());
        assert_eq!(text[255], 0xC3, "the test must cut the character at 256");
        fs::write(&cut, &text).unwrap();
        assert!(is_shell_script(&cut));

        // A first line that is not UTF-8 at all is not a shebang.
        let binary = tmp.path().join("binary");
        fs::write(&binary, b"#!/bin/sh\xff\xfe\nx\n").unwrap();
        assert!(!is_shell_script(&binary));
    }

    #[test]
    fn shebang_names_its_shell() {
        // The shell a shebang asks for, exactly as written — the `env` form
        // names the shell rather than `env` itself.
        assert_eq!(shebang_shell("#!/bin/bash"), Some("/bin/bash"));
        assert_eq!(shebang_shell("#!/usr/bin/env bash"), Some("bash"));
        assert_eq!(shebang_shell("#!/bin/sh"), Some("/bin/sh"));
        assert_eq!(shebang_shell("#! /bin/sh -e"), Some("/bin/sh"));
        for shell in ["zsh", "dash", "ash", "ksh"] {
            let direct = format!("#!/bin/{shell}");
            assert_eq!(shebang_shell(&direct), Some(&direct[2..]));
            let env = format!("#!/usr/bin/env {shell}");
            assert_eq!(shebang_shell(&env), Some(shell));
        }
        // Not a shell, not a shebang at all, or a shebang naming nothing.
        for line in [
            "#!/usr/bin/python3",
            "#!/usr/bin/env ruby",
            "#!/usr/bin/env node",
            "echo hi",
            "#!",
            "#!/usr/bin/env",
        ] {
            assert_eq!(shebang_shell(line), None, "{line:?}");
        }
    }

    #[test]
    fn script_shell_resolves_the_shell_its_shebang_names() {
        // The shebang decides, and the shell it names is used as written.
        assert_eq!(script_shell("#!/bin/sh\necho hi\n"), "/bin/sh");
        // A recognized shell that is not installed falls back to /bin/sh,
        // rather than failing the exec of every test in the file.
        assert_eq!(script_shell("#!/no/such/bash\necho hi\n"), "/bin/sh");
        // No shebang, or one naming something that is not a shell: bash.
        for source in ["echo hi\n", "#!/usr/bin/python3\nprint('hi')\n", ""] {
            assert_eq!(script_shell(source), resolve_shell("bash"), "{source:?}");
        }
    }

    #[test]
    fn resolve_shell_falls_back_when_missing() {
        assert_eq!(resolve_shell("/bin/sh"), "/bin/sh");
        assert_eq!(resolve_shell("/no/such/shell"), "/bin/sh");
        assert_eq!(resolve_shell("definitely-not-a-real-shell-xyz"), "/bin/sh");
    }

    #[test]
    fn resolve_shell_turns_a_bare_name_into_a_path() {
        // The runner execs the answer with execv, which searches no PATH: a
        // bare name (`#!/usr/bin/env bash`, or no shebang at all) has to come
        // back as the path PATH resolves it to.
        let sh = resolve_shell("sh");
        assert!(Path::new(&sh).is_absolute(), "{sh:?}");
        assert_eq!(Path::new(&sh), which::which("sh").unwrap());
        let env_sh = script_shell("#!/usr/bin/env sh\necho hi\n");
        assert!(Path::new(&env_sh).is_absolute(), "{env_sh:?}");
    }
}

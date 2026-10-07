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
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
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

    read_first_line(path).is_some_and(|line| is_shell_interpreter(&line))
}

/// Reads the first line of a file (up to 256 bytes). Returns `None` if the file
/// cannot be opened or read.
fn read_first_line(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 256];
    let n = file.read(&mut buf).ok()?;
    let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
    Some(head.lines().next().unwrap_or("").to_string())
}

/// Extracts the interpreter token from a shebang line, handling the
/// `#!/usr/bin/env bash` form. Returns `None` when the line is not a shebang.
fn shebang_interpreter(line: &str) -> Option<&str> {
    let mut parts = line.strip_prefix("#!")?.split_whitespace();
    let first = parts.next()?;
    if first.ends_with("/env") {
        parts.next()
    } else {
        Some(first)
    }
}

/// Returns the shell interpreter to use for a script file. Reads the shebang
/// line and extracts the interpreter; falls back to "bash" if absent or unrecognized.
pub(crate) fn get_script_shell(path: &Path) -> String {
    read_first_line(path)
        .filter(|line| is_shell_interpreter(line))
        .and_then(|line| shebang_interpreter(&line).map(str::to_string))
        .unwrap_or_else(|| "bash".to_string())
}

pub(crate) fn is_shell_interpreter(shebang: &str) -> bool {
    let Some(interpreter) = shebang_interpreter(shebang) else {
        return false;
    };
    let basename = interpreter.rsplit('/').next().unwrap_or(interpreter);
    matches!(basename, "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh")
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
    fn discover_directory_finds_shell_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.sh"), "#!/bin/bash\n").unwrap();
        fs::write(tmp.path().join("b.test"), "#!/bin/bash\n").unwrap();
        fs::write(tmp.path().join("c.txt"), "not a script\n").unwrap();

        let result = discover_test_files(tmp.path()).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|p| p.ends_with("a.sh")));
        assert!(result.iter().any(|p| p.ends_with("b.test")));
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
    fn discover_results_are_sorted() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("z.sh"), "#!/bin/bash\n").unwrap();
        fs::write(tmp.path().join("a.sh"), "#!/bin/bash\n").unwrap();
        fs::write(tmp.path().join("m.sh"), "#!/bin/bash\n").unwrap();

        let result = discover_test_files(tmp.path()).unwrap();
        let sorted: Vec<_> = {
            let mut v = result.clone();
            v.sort();
            v
        };
        assert_eq!(result, sorted);
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
    }

    #[test]
    fn script_shell_from_shebang() {
        let tmp = TempDir::new().unwrap();

        let bash_file = tmp.path().join("bash.sh");
        fs::write(&bash_file, "#!/bin/bash\necho hi\n").unwrap();
        assert_eq!(get_script_shell(&bash_file), "/bin/bash");

        let env_bash = tmp.path().join("env_bash.sh");
        fs::write(&env_bash, "#!/usr/bin/env bash\necho hi\n").unwrap();
        assert_eq!(get_script_shell(&env_bash), "bash");

        let sh_file = tmp.path().join("sh.sh");
        fs::write(&sh_file, "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(get_script_shell(&sh_file), "/bin/sh");

        let no_shebang = tmp.path().join("noshebang.sh");
        fs::write(&no_shebang, "echo hi\n").unwrap();
        assert_eq!(get_script_shell(&no_shebang), "bash");

        let python_file = tmp.path().join("python.py");
        fs::write(&python_file, "#!/usr/bin/python3\nprint('hi')\n").unwrap();
        assert_eq!(get_script_shell(&python_file), "bash");
    }

    #[test]
    fn interpreter_detection() {
        assert!(is_shell_interpreter("#!/bin/sh"));
        assert!(is_shell_interpreter("#!/bin/bash"));
        assert!(is_shell_interpreter("#!/usr/bin/env bash"));
        assert!(is_shell_interpreter("#!/usr/bin/env zsh"));
        assert!(is_shell_interpreter("#!/bin/dash"));
        assert!(is_shell_interpreter("#!/bin/ash"));
        assert!(is_shell_interpreter("#!/usr/bin/env ksh"));
        assert!(!is_shell_interpreter("#!/usr/bin/python3"));
        assert!(!is_shell_interpreter("#!/usr/bin/env ruby"));
        assert!(!is_shell_interpreter("#!/usr/bin/env node"));
    }
}

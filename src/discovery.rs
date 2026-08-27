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
        if path.is_dir() {
            collect_script_files(&path, files)?;
        } else if is_shell_script(&path) {
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

/// Returns the shebang's interpreter token if it names a recognized shell.
fn shebang_shell(line: &str) -> Option<&str> {
    let interpreter = shebang_interpreter(line)?;
    let basename = interpreter.rsplit('/').next().unwrap_or(interpreter);
    matches!(basename, "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh").then_some(interpreter)
}

/// Returns the shell interpreter to use for a script file. Reads the shebang
/// line and extracts the interpreter; falls back to "bash" if absent or unrecognized.
pub(crate) fn get_script_shell(path: &Path) -> String {
    read_first_line(path)
        .and_then(|line| shebang_shell(&line).map(str::to_string))
        .unwrap_or_else(|| "bash".to_string())
}

pub(crate) fn is_shell_interpreter(shebang: &str) -> bool {
    shebang_shell(shebang).is_some()
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

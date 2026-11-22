//! Fork-and-exec with the parent kept awake while the child gets there.
//!
//! `std::process::Command` is the obvious tool, and the runner used it. It
//! stopped fitting once a test could be watched through seccomp (see
//! `seccomp`): the filter is installed by the child between `fork` and
//! `exec`, the child's `execve` is then a syscall the filter may well be
//! watching, and a watched syscall does not complete until the parent has
//! answered its notification — while `Command::spawn` has the parent blocked
//! on a pipe waiting for that very `execve` to go through. Each side waiting
//! on the other, for every test that asks about `execve`.
//!
//! So the fork is done here, the same way `std` does it — hooks run in the
//! child in order, a close-on-exec pipe carries a failing hook's errno back,
//! and EOF on it means the exec went through — but the parent waits on that
//! pipe with [`Spawning::wait_exec`], which answers the child's seccomp
//! listener in the meantime. A filter given to [`Spawn::watch`] is installed
//! after every hook, and [`Spawn::start`] takes its listener out of the child
//! before returning, so the whole handoff happens here rather than at each
//! caller.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use crate::seccomp::{Filter, Handoff, Listener, pollin};

/// Something run in the child between `fork` and `exec`. Only
/// async-signal-safe calls, no allocation: the parent has other threads, and
/// the child is a copy of the process at a moment one of them may hold a lock.
pub type Hook = Box<dyn FnMut() -> io::Result<()>>;

/// What to run and how.
pub struct Spawn {
    program: CString,
    argv: Vec<CString>,
    stdout: File,
    stderr: File,
    hooks: Vec<Hook>,
    /// Variables set in the child on top of the inherited environment.
    env: Vec<(String, String)>,
    /// The seccomp filter to install after the hooks, and the page its
    /// listener is handed over through.
    watch: Option<(Filter, Handoff)>,
}

impl Spawn {
    /// `argv` is the whole vector, `argv[0]` included, given separately from
    /// `program`. The runner passes the shell's own path as `argv[0]` and
    /// sets `$0` to the test file through the operand after `-c`'s script:
    /// a multi-call binary such as busybox picks what to run from `argv[0]`,
    /// so a shell exec'd under any other name may not be a shell at all.
    pub fn new(program: &str, argv: &[&str], stdout: File, stderr: File) -> io::Result<Self> {
        let cstring = |s: &str| {
            CString::new(s)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in argument"))
        };
        Ok(Self {
            program: cstring(program)?,
            argv: argv.iter().map(|a| cstring(a)).collect::<io::Result<_>>()?,
            stdout,
            stderr,
            hooks: Vec::new(),
            env: Vec::new(),
            watch: None,
        })
    }

    /// Set `key` to `value` in the child's environment, which is otherwise
    /// the parent's.
    pub fn env(&mut self, key: &str, value: &str) -> &mut Self {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// The child's environment as `execve` wants it: the parent's, with
    /// every variable [`Self::env`] set replacing any of the same name.
    fn envp(&self) -> io::Result<Vec<CString>> {
        use std::os::unix::ffi::OsStrExt;
        let overridden = |k: &[u8]| self.env.iter().any(|(o, _)| o.as_bytes() == k);
        let mut envp = Vec::new();
        for (k, v) in std::env::vars_os() {
            if overridden(k.as_bytes()) {
                continue;
            }
            let mut entry = k.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(v.as_bytes());
            // A NUL cannot be in an inherited variable; skip rather than fail.
            if let Ok(c) = CString::new(entry) {
                envp.push(c);
            }
        }
        for (k, v) in &self.env {
            envp.push(
                CString::new(format!("{k}={v}")).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "NUL in environment")
                })?,
            );
        }
        Ok(envp)
    }

    /// Run `hook` in the child, after every hook added before it.
    pub fn hook(&mut self, hook: impl FnMut() -> io::Result<()> + 'static) -> &mut Self {
        self.hooks.push(Box::new(hook));
        self
    }

    /// Watch the child's syscalls through `filter`: installed in the child
    /// after every hook, so none of their syscalls are in the count, with
    /// its listener taken out of the child by [`Self::start`] and answered
    /// by [`Spawning::wait_exec`] until the exec goes through.
    pub fn watch(&mut self, filter: Filter) -> io::Result<&mut Self> {
        self.watch = Some((filter, Handoff::new()?));
        Ok(self)
    }

    /// Fork. The child runs the hooks, installs the [`Self::watch`] filter if
    /// there is one, and execs; the parent gets back as soon as it holds the
    /// filter's listener (at once, without one), with the pipe the child's
    /// outcome arrives on.
    ///
    /// Failing to take the listener is an error of its own: the child,
    /// which would otherwise spin waiting for it, has been killed and reaped.
    pub fn start(mut self) -> io::Result<Spawning> {
        // argv and envp as the `char *const []` execve wants, built before
        // the fork.
        let mut argv_ptrs: Vec<*const libc::c_char> =
            self.argv.iter().map(|a| a.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let envp = self.envp()?;
        let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
        envp_ptrs.push(std::ptr::null());

        // Both ends close-on-exec, so EOF on the read end is the exec.
        let (status_rd, status_wr) = io::pipe()?;

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // The child. Nothing below allocates or returns: whatever goes
            // wrong is reported through the pipe and ends in `_exit`.
            let fail = |err: io::Error| -> ! {
                let errno = err.raw_os_error().unwrap_or(0);
                let bytes = errno.to_ne_bytes();
                unsafe {
                    libc::write(status_wr.as_raw_fd(), bytes.as_ptr().cast(), bytes.len());
                    libc::_exit(127);
                }
            };
            unsafe {
                if libc::dup2(self.stdout.as_raw_fd(), libc::STDOUT_FILENO) < 0
                    || libc::dup2(self.stderr.as_raw_fd(), libc::STDERR_FILENO) < 0
                {
                    fail(io::Error::last_os_error());
                }
                // Rust ignores SIGPIPE at startup, and an ignored disposition
                // survives exec — `Command` puts it back in the child, so
                // this has to as well. Left ignored, a test's whole tree
                // inherits it and no shell can undo it (a non-interactive
                // shell may not trap a signal ignored on entry): `yes | head
                // -1` complains of a broken pipe instead of dying quietly,
                // and a writer that shrugs off the `EPIPE` (a loop of `echo`
                // outside `set -e`) never ends.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
            for hook in &mut self.hooks {
                if let Err(e) = hook() {
                    fail(e);
                }
            }
            if let Some((filter, handoff)) = &self.watch
                && let Err(e) = unsafe { filter.install_and_hand_off(handoff) }
            {
                fail(e);
            }
            unsafe {
                libc::execve(
                    self.program.as_ptr(),
                    argv_ptrs.as_ptr(),
                    envp_ptrs.as_ptr(),
                )
            };
            fail(io::Error::last_os_error());
        }

        drop(status_wr);
        let mut child = Child { pid, status: None };
        // `None` from `take` is a child that ended without publishing a
        // listener; why is on the status pipe, which `wait_exec` reads.
        let listener = match &self.watch {
            Some((_, handoff)) => handoff.take(pid).map_err(|e| {
                let _ = child.wait();
                io::Error::new(e.kind(), format!("taking the seccomp listener: {e}"))
            })?,
            None => None,
        };
        Ok(Spawning {
            child,
            status_rd,
            listener,
        })
    }
}

/// A forked child that has not yet been seen to exec.
pub struct Spawning {
    child: Child,
    status_rd: io::PipeReader,
    listener: Option<Listener>,
}

impl Spawning {
    /// Wait for the child to exec — or to fail before it does, in which case
    /// the child is reaped and its hook's error returned. On success, the
    /// child comes back with its seccomp listener, if it was watched.
    ///
    /// The listener is serviced while this waits: the exec may itself be a
    /// watched syscall, held until the parent answers it.
    pub fn wait_exec(mut self) -> io::Result<(Child, Option<Listener>)> {
        let mut listener = self.listener.take();
        // A negative descriptor is one `poll` ignores.
        let listener_fd = listener.as_ref().map_or(-1, Listener::poll_fd);
        let mut fds = [pollin(self.status_rd.as_raw_fd()), pollin(listener_fd)];
        loop {
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if fds[1].revents != 0
                && let Some(l) = listener.as_mut()
            {
                l.service();
                // A listener with nobody left behind it stays in `POLLHUP`.
                fds[1].fd = l.poll_fd();
            }
            if fds[0].revents == 0 {
                continue;
            }
            // The outcome: EOF for an exec that went through, an errno for
            // a hook or exec that did not. The child writes the errno in one
            // `write` of fewer than `PIPE_BUF` bytes, which a pipe delivers
            // whole, so there is no such thing as part of one.
            let mut errno = [0u8; 4];
            return match self.status_rd.read_exact(&mut errno) {
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok((self.child, listener)),
                Err(e) => Err(e),
                Ok(()) => {
                    let _ = self.child.wait();
                    Err(io::Error::from_raw_os_error(i32::from_ne_bytes(errno)))
                }
            };
        }
    }
}

/// A child process, by pid. The pid is only ever used while the child is
/// unreaped — after that the kernel may hand it to anybody — which is what
/// `status` records.
#[derive(Debug)]
pub struct Child {
    pid: libc::pid_t,
    status: Option<ExitStatus>,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    /// Has the child been reaped? From then on its pid is not ours to signal.
    pub fn reaped(&self) -> bool {
        self.status.is_some()
    }

    /// Has the child exited? `waitid` with `WNOWAIT` answers that without
    /// consuming the zombie, so the pid — and a process group named after
    /// it — stays reserved until [`Self::wait`] reaps it.
    pub fn has_exited(&self) -> io::Result<bool> {
        if self.reaped() {
            return Ok(true);
        }
        // Zeroed up front because `waitid` leaves the struct untouched when
        // `WNOHANG` finds nothing, and `si_pid == 0` is how that is reported.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if ret == -1 {
            let err = io::Error::last_os_error();
            // A signal cut the call short; the caller asks again.
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err);
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    /// Reap the child, waiting for it to exit. Once [`Self::has_exited`] has
    /// said it has, this does not block.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        loop {
            let mut raw = 0;
            if unsafe { libc::waitpid(self.pid, &mut raw, 0) } < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            let status = ExitStatus::from_raw(raw);
            self.status = Some(status);
            return Ok(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn logs() -> (tempfile::TempDir, File, File) {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = File::create(tmp.path().join("out")).unwrap();
        let err = File::create(tmp.path().join("err")).unwrap();
        (tmp, out, err)
    }

    /// `$0` is set the way the runner sets it, through the operand after
    /// `-c`'s script, rather than through `argv[0]`: a multi-call `/bin/sh`
    /// (busybox, which is what Nix's build sandbox provides) picks its applet
    /// from `argv[0]` and refuses to run under any other name.
    #[test]
    fn runs_the_program_with_its_argv_and_logs() {
        let (tmp, out, err) = logs();
        let spawn = Spawn::new(
            "/bin/sh",
            &["sh", "-c", "echo $0 $1; echo oops >&2", "custom0", "arg1"],
            out,
            err,
        )
        .unwrap();
        let (mut child, _) = spawn.start().unwrap().wait_exec().unwrap();
        assert!(child.wait().unwrap().success());
        let mut s = String::new();
        File::open(tmp.path().join("out"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "custom0 arg1\n");
        s.clear();
        File::open(tmp.path().join("err"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "oops\n");
    }

    /// A variable set with `env` reaches the child, replacing one of the same
    /// name, and the rest of the parent's environment is still inherited.
    #[test]
    fn env_overrides_on_top_of_the_inherited_environment() {
        let (tmp, out, err) = logs();
        let mut spawn = Spawn::new(
            "/bin/sh",
            &["sh", "-c", "echo \"$ATTEST_SPAWN_X|${PATH:+inherited}\""],
            out,
            err,
        )
        .unwrap();
        spawn
            .env("ATTEST_SPAWN_X", "first")
            .env("ATTEST_SPAWN_X", "second");
        let (mut child, _) = spawn.start().unwrap().wait_exec().unwrap();
        assert!(child.wait().unwrap().success());
        let s = std::fs::read_to_string(tmp.path().join("out")).unwrap();
        assert_eq!(s, "second|inherited\n");
    }

    /// The child starts with SIGPIPE at its default, not ignored the way the
    /// Rust runtime (this test binary's included) leaves it in the parent:
    /// a shell sending itself one dies of it.
    #[test]
    fn the_child_dies_of_sigpipe() {
        let (_tmp, out, err) = logs();
        let spawn =
            Spawn::new("/bin/sh", &["sh", "-c", "kill -PIPE $$; exit 0"], out, err).unwrap();
        let (mut child, _) = spawn.start().unwrap().wait_exec().unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGPIPE));
    }

    #[test]
    fn a_failing_hook_is_reported_and_the_child_reaped() {
        let (_tmp, out, err) = logs();
        let mut spawn = Spawn::new("/bin/sh", &["sh", "-c", "true"], out, err).unwrap();
        spawn.hook(|| Err(io::Error::from_raw_os_error(libc::EPERM)));
        let err = spawn.start().unwrap().wait_exec().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn a_missing_program_is_an_exec_error() {
        let (_tmp, out, err) = logs();
        let spawn = Spawn::new("/nonexistent/prog", &["prog"], out, err).unwrap();
        let err = spawn.start().unwrap().wait_exec().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    /// `has_exited` sees the exit without reaping: the pid stays the child's
    /// until `wait` consumes the zombie, and only then is it `reaped`.
    #[test]
    fn has_exited_leaves_the_zombie_for_wait() {
        let (_tmp, out, err) = logs();
        let spawn = Spawn::new("/bin/sh", &["sh", "-c", "exit 3"], out, err).unwrap();
        let (mut child, _) = spawn.start().unwrap().wait_exec().unwrap();
        while !child.has_exited().unwrap() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!child.reaped());
        // Still a zombie of ours: signalling it is not an error.
        assert_eq!(unsafe { libc::kill(child.id() as i32, 0) }, 0);
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(3));
        assert!(child.reaped());
        assert!(child.has_exited().unwrap());
    }

    /// Hooks run in the order they were added, in the child.
    #[test]
    fn hooks_run_in_order_before_the_exec() {
        let (tmp, out, err) = logs();
        let marker = CString::new(tmp.path().join("marker").to_str().unwrap()).unwrap();
        let mut spawn = Spawn::new(
            "/bin/sh",
            &["sh", "-c", "test -e \"$1\"", "sh", marker.to_str().unwrap()],
            out,
            err,
        )
        .unwrap();
        let m1 = marker.clone();
        spawn.hook(move || {
            // Created here so the shell can see it; stays async-signal-safe.
            let fd = unsafe { libc::open(m1.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            unsafe { libc::close(fd) };
            Ok(())
        });
        let (mut child, _) = spawn.start().unwrap().wait_exec().unwrap();
        assert!(child.wait().unwrap().success());
    }
}

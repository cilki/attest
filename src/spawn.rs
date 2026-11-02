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
//! pipe with [`Spawning::wait_exec`], which takes something else to service
//! in the meantime.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

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
}

impl Spawn {
    /// `argv[0]` is given separately from `program`: the runner execs a
    /// shell under the test file's own name so `$0` works.
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
        })
    }

    /// Run `hook` in the child, after every hook added before it.
    pub fn hook(&mut self, hook: impl FnMut() -> io::Result<()> + 'static) -> &mut Self {
        self.hooks.push(Box::new(hook));
        self
    }

    /// Fork. The child runs the hooks and execs; the parent gets back at
    /// once, with the child's pid and the pipe its outcome arrives on.
    pub fn start(mut self) -> io::Result<Spawning> {
        // argv as the `char *const []` execv wants, built before the fork.
        let mut argv_ptrs: Vec<*const libc::c_char> =
            self.argv.iter().map(|a| a.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let [status_rd, status_wr] = fds;
        let status_rd = unsafe { OwnedFd::from_raw_fd(status_rd) };
        let status_wr = unsafe { OwnedFd::from_raw_fd(status_wr) };

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
            }
            for hook in &mut self.hooks {
                if let Err(e) = hook() {
                    fail(e);
                }
            }
            unsafe { libc::execv(self.program.as_ptr(), argv_ptrs.as_ptr()) };
            fail(io::Error::last_os_error());
        }

        drop(status_wr);
        Ok(Spawning {
            child: Child { pid, status: None },
            status_rd,
        })
    }
}

/// A forked child that has not yet been seen to exec.
pub struct Spawning {
    child: Child,
    status_rd: OwnedFd,
}

impl Spawning {
    /// Wait for the child to exec — or to fail before it does, in which case
    /// the child is reaped and its hook's error returned.
    ///
    /// `service` is called with the descriptors in `also` that are readable
    /// while this waits, so the parent can keep answering whatever the child
    /// is blocked on; `also` may be empty.
    pub fn wait_exec(
        mut self,
        also: &[RawFd],
        mut service: impl FnMut(RawFd),
    ) -> io::Result<Child> {
        let mut fds: Vec<libc::pollfd> = std::iter::once(self.status_rd.as_raw_fd())
            .chain(also.iter().copied())
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        loop {
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            for pfd in &fds[1..] {
                if pfd.revents != 0 {
                    service(pfd.fd);
                }
            }
            if fds[0].revents == 0 {
                continue;
            }
            // The outcome: EOF for an exec that went through, an errno for
            // a hook or exec that did not.
            let mut buf = [0u8; 4];
            let mut got = 0;
            while got < buf.len() {
                let n = unsafe {
                    libc::read(
                        self.status_rd.as_raw_fd(),
                        buf[got..].as_mut_ptr().cast(),
                        buf.len() - got,
                    )
                };
                match n {
                    0 => break,
                    n if n > 0 => got += n as usize,
                    _ => {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
            if got == 0 {
                return Ok(self.child);
            }
            let errno = i32::from_ne_bytes(buf);
            let _ = self.child.wait();
            return Err(io::Error::from_raw_os_error(errno));
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
    /// it — stays reserved until [`Self::try_wait`] or [`Self::wait`] reaps
    /// it.
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

    /// `SIGKILL` the child. Nothing happens to one already reaped.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.reaped() {
            return Ok(());
        }
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn waitpid(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        loop {
            let mut raw = 0;
            let pid = unsafe { libc::waitpid(self.pid, &mut raw, flags) };
            if pid < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if pid == 0 {
                return Ok(None);
            }
            let status = ExitStatus::from_raw(raw);
            self.status = Some(status);
            return Ok(Some(status));
        }
    }

    /// Reap the child if it has exited; `None` while it is still running.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.waitpid(libc::WNOHANG)
    }

    /// Reap the child, waiting for it to exit.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(self
            .waitpid(0)?
            .expect("a blocking waitpid returns a status"))
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

    #[test]
    fn runs_the_program_with_its_argv_and_logs() {
        let (tmp, out, err) = logs();
        let spawn = Spawn::new(
            "/bin/sh",
            &["custom0", "-c", "echo $0; echo oops >&2"],
            out,
            err,
        )
        .unwrap();
        let mut child = spawn.start().unwrap().wait_exec(&[], |_| {}).unwrap();
        assert!(child.wait().unwrap().success());
        let mut s = String::new();
        File::open(tmp.path().join("out"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "custom0\n");
        s.clear();
        File::open(tmp.path().join("err"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "oops\n");
    }

    #[test]
    fn a_failing_hook_is_reported_and_the_child_reaped() {
        let (_tmp, out, err) = logs();
        let mut spawn = Spawn::new("/bin/sh", &["sh", "-c", "true"], out, err).unwrap();
        spawn.hook(|| Err(io::Error::from_raw_os_error(libc::EPERM)));
        let err = spawn.start().unwrap().wait_exec(&[], |_| {}).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn a_missing_program_is_an_exec_error() {
        let (_tmp, out, err) = logs();
        let spawn = Spawn::new("/nonexistent/prog", &["prog"], out, err).unwrap();
        let err = spawn.start().unwrap().wait_exec(&[], |_| {}).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    /// `has_exited` sees the exit without reaping: the pid stays the child's
    /// until `try_wait` consumes the zombie, and only then is it `reaped`.
    #[test]
    fn has_exited_leaves_the_zombie_for_try_wait() {
        let (_tmp, out, err) = logs();
        let spawn = Spawn::new("/bin/sh", &["sh", "-c", "exit 3"], out, err).unwrap();
        let mut child = spawn.start().unwrap().wait_exec(&[], |_| {}).unwrap();
        while !child.has_exited().unwrap() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!child.reaped());
        // Still a zombie of ours: signalling it is not an error.
        assert_eq!(unsafe { libc::kill(child.id() as i32, 0) }, 0);
        let status = child.try_wait().unwrap().expect("exited");
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
        let mut child = spawn.start().unwrap().wait_exec(&[], |_| {}).unwrap();
        assert!(child.wait().unwrap().success());
    }

    /// `wait_exec` keeps servicing the extra descriptors while the child is
    /// on its way to exec.
    #[test]
    fn wait_exec_services_other_descriptors_meanwhile() {
        let (_tmp, out, err) = logs();
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let [rd, wr] = fds;
        let mut spawn = Spawn::new("/bin/sh", &["sh", "-c", "true"], out, err).unwrap();
        spawn.hook(move || {
            // Say something the parent has to read before the exec: it is
            // written in the child, so the parent's end is only readable
            // while the child is between fork and exec.
            unsafe { libc::write(wr, b"x".as_ptr().cast(), 1) };
            Ok(())
        });
        let mut seen = Vec::new();
        let mut child = spawn
            .start()
            .unwrap()
            .wait_exec(&[rd], |fd| {
                let mut b = [0u8; 1];
                unsafe { libc::read(fd, b.as_mut_ptr().cast(), 1) };
                seen.push((fd, b[0]));
            })
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(seen, [(rd, b'x')]);
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }
    }
}

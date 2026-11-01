//! Watching a test's system calls with seccomp user notification.
//!
//! A test with a `syscalls` condition gets a seccomp filter installed in its
//! child before the exec, so the whole process tree inherits it. The filter
//! returns `SECCOMP_RET_USER_NOTIF` for the syscalls the condition is about
//! — those in a `not in` set, or everything *outside* an `in` set — and
//! `SECCOMP_RET_ALLOW` for the rest, which therefore cost nothing. Each
//! notification reaches attest over the listener descriptor the kernel hands
//! back when the filter is installed; attest records the number and answers
//! `SECCOMP_USER_NOTIF_FLAG_CONTINUE`, which lets the syscall run exactly as
//! it would have. Nothing is blocked: a condition is an assertion on what
//! the test did, checked after it has done it, the same as `cpu < 10ms`.
//!
//! The filter is installed last, after the overlay is set up, so attest's own
//! `mount`s and `pivot_root` are not counted — but the shell's startup is,
//! the way its startup CPU time is: `execve`, `brk`, `mmap` and the rest are
//! the shell's, not the test's, and an allow-list has to admit them.
//!
//! Installing a filter needs either `CAP_SYS_ADMIN` in the caller's user
//! namespace or `no_new_privs`. Every test has the former — the overlay was
//! mounted with it — so `no_new_privs` is only set as a fallback, since it
//! would stop a test from gaining privileges through a setuid binary.
//!
//! `CONTINUE` needs Linux 5.5. [`probe`] finds out once per run whether the
//! whole round trip works; where it does not, no filter is installed and the
//! condition is reported as unmeasured rather than quietly passed.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::decorator::{SyscallLog, Sysno};

/// The `arch` field a native syscall carries in `seccomp_data`
/// (`AUDIT_ARCH_*`). The filter allows any other ABI's syscalls unseen: the
/// numbers in the condition were resolved against this one's table, and a
/// 32-bit compat syscall with the same number is a different call.
#[cfg(target_arch = "x86_64")]
const NATIVE_ARCH: u32 = 0xc000003e;
#[cfg(target_arch = "aarch64")]
const NATIVE_ARCH: u32 = 0xc00000b7;
#[cfg(target_arch = "riscv64")]
const NATIVE_ARCH: u32 = 0xc00000f3;
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
)))]
const NATIVE_ARCH: u32 = 0;

/// A classic BPF program for the kernel's seccomp filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    prog: Vec<Instruction>,
}

/// One `sock_filter` as plain data, so a program can be compared and built
/// without `libc`'s struct in the way. `repr(C)` because the kernel reads
/// it as that struct.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Instruction {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const fn stmt(code: u32, k: u32) -> Instruction {
    Instruction {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jump(code: u32, k: u32, jt: u8, jf: u8) -> Instruction {
    Instruction {
        code: code as u16,
        jt,
        jf,
        k,
    }
}

impl Filter {
    /// A filter notifying on each of `watched` and allowing everything else.
    /// `None` for an empty set, or on an architecture this module does not
    /// know the audit number of: there is nothing to install.
    pub fn watching(watched: &[Sysno]) -> Option<Self> {
        if watched.is_empty() || NATIVE_ARCH == 0 {
            return None;
        }
        let mut set: Vec<u32> = watched.iter().map(|nr| nr.id() as u32).collect();
        set.sort_unstable();
        set.dedup();

        let arch_offset = std::mem::offset_of!(libc::seccomp_data, arch) as u32;
        let nr_offset = std::mem::offset_of!(libc::seccomp_data, nr) as u32;
        let mut prog = vec![
            // Not this ABI: allow, unseen.
            stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, arch_offset),
            jump(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                NATIVE_ARCH,
                1,
                0,
            ),
            stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW),
            stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, nr_offset),
        ];
        // Each watched number is a compare followed by its own return, so no
        // jump ever has to reach further than the next instruction: a BPF
        // jump offset is a byte, and a complement set has hundreds of
        // entries.
        for nr in set {
            prog.push(jump(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, nr, 0, 1));
            prog.push(stmt(
                libc::BPF_RET | libc::BPF_K,
                libc::SECCOMP_RET_USER_NOTIF,
            ));
        }
        prog.push(stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW));
        Some(Filter { prog })
    }

    /// The number of instructions, for a test to check the shape.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.prog.len()
    }

    /// Install this filter on the calling thread and send the listener the
    /// kernel returns for it over `sock`. For the child, between fork and
    /// exec: nothing here allocates.
    ///
    /// # Safety
    /// `sock` must be the writing end of a `SOCK_STREAM` socketpair whose
    /// other end the parent reads with [`receive`].
    pub unsafe fn install_and_send(&self, sock: RawFd) -> io::Result<()> {
        let mut fprog = libc::sock_fprog {
            len: self.prog.len() as libc::c_ushort,
            // Same layout, by construction.
            filter: self.prog.as_ptr() as *mut libc::sock_filter,
        };
        let seccomp = |fprog: &mut libc::sock_fprog| unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
                fprog as *mut libc::sock_fprog,
            )
        };
        let mut fd = seccomp(&mut fprog);
        if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EACCES) {
            // No CAP_SYS_ADMIN here after all: the other way in.
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
            fd = seccomp(&mut fprog);
        }
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = fd as RawFd;
        let sent = unsafe { send_fd(sock, fd) };
        unsafe { libc::close(fd) };
        sent
    }
}

/// Send `fd` over the stream socket `sock` as ancillary data, with one byte
/// of payload for it to ride on. Async-signal-safe: stack buffers only.
unsafe fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    unsafe {
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        // Big enough for the one descriptor, aligned like a cmsghdr.
        let mut control = [0u64; 4];
        debug_assert!(space <= std::mem::size_of_val(&control));
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Receive the descriptor a child sent with [`Filter::install_and_send`]
/// over the parent's end of the socketpair. `None` when the child closed
/// its end without sending: it failed before it got that far, which the
/// exec-status pipe says more about.
pub fn receive(sock: &OwnedFd) -> io::Result<Option<Listener>> {
    unsafe {
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u64; 4];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        let n = loop {
            let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            break n;
        };
        if n == 0 {
            return Ok(None);
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other("no descriptor in the message"));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
        Ok(Some(Listener {
            fd: OwnedFd::from_raw_fd(fd),
            log: SyscallLog::default(),
            dead: false,
        }))
    }
}

/// The parent's end of one test's filter: the notifications arrive here,
/// and the log of what they were about builds up.
pub struct Listener {
    fd: OwnedFd,
    log: SyscallLog,
    /// Set once the kernel has said there is nobody left behind the filter.
    dead: bool,
}

impl Listener {
    /// The descriptor to poll for `POLLIN`; `None` once the listener is dead
    /// and there is nothing left to hear.
    pub fn poll_fd(&self) -> Option<RawFd> {
        (!self.dead).then(|| self.fd.as_raw_fd())
    }

    /// Answer every notification that is pending: record its syscall and
    /// let it continue.
    ///
    /// The receive is only attempted while the descriptor says it is
    /// readable, since `SECCOMP_IOCTL_NOTIF_RECV` blocks otherwise — and a
    /// test whose every process is stopped under `--fuzz` makes none.
    pub fn service(&mut self) {
        while !self.dead {
            let revents = self.poll_once();
            if revents & libc::POLLIN != 0 {
                self.handle_one();
            } else if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                // Nobody is behind the filter any more.
                self.dead = true;
            } else {
                break;
            }
        }
    }

    /// What the descriptor reports right now, without waiting.
    fn poll_once(&self) -> libc::c_short {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
            pfd.revents
        } else {
            0
        }
    }

    fn handle_one(&mut self) {
        // The kernel insists on a zeroed struct to receive into.
        let mut notif: libc::seccomp_notif = unsafe { std::mem::zeroed() };
        let got = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_RECV,
                &mut notif,
            )
        };
        if got < 0 {
            match io::Error::last_os_error().raw_os_error() {
                // The notifying task died before this got to it, or a
                // signal cut the call short: the next poll says what is
                // left.
                Some(libc::ENOENT) | Some(libc::EINTR) => {}
                _ => self.dead = true,
            }
            return;
        }
        if let Some(nr) = Sysno::new(notif.data.nr as usize) {
            self.log.record(nr);
        }
        let resp = libc::seccomp_notif_resp {
            id: notif.id,
            val: 0,
            error: 0,
            flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
        };
        // ENOENT here is the task having died meanwhile; nothing to do.
        unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_SEND, &resp) };
    }

    /// What was heard, once the test is over. Anything still pending is
    /// taken first.
    pub fn into_log(mut self) -> SyscallLog {
        self.service();
        self.log
    }
}

/// A `SOCK_STREAM` socketpair, both ends close-on-exec: the child's end
/// rides through the fork and is closed by the exec, the parent's end is
/// read with [`receive`].
pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Does the whole round trip work here — install, notify, `CONTINUE`?
///
/// A child installs a filter watching `getppid`, calls it, and exits 0 only
/// if the call still returned its parent's pid; the parent answers the one
/// notification. Anything short of that — `seccomp(2)` refusing the
/// listener flag, `CONTINUE` refused (Linux before 5.5), the child never
/// reporting — is the error, which says which.
pub fn probe() -> Result<(), String> {
    let Some(filter) = Filter::watching(&[Sysno::getppid]) else {
        return Err("no filter for this architecture".to_string());
    };
    let (parent_end, child_end) = socketpair().map_err(|e| format!("socketpair: {e}"))?;
    let me = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            let code = match filter.install_and_send(child_end.as_raw_fd()) {
                Ok(()) => i32::from(libc::getppid() != me),
                Err(e) => 100 + e.raw_os_error().unwrap_or(0).clamp(0, 100),
            };
            libc::_exit(code);
        }
    }
    drop(child_end);
    let exit_code = |status: libc::c_int| -> Result<(), String> {
        if !libc::WIFEXITED(status) {
            return Err("the child was killed".to_string());
        }
        match libc::WEXITSTATUS(status) {
            0 => Ok(()),
            1 => Err("CONTINUE did not let the syscall through".to_string()),
            code => Err(format!(
                "seccomp(2): {}",
                io::Error::from_raw_os_error(code - 100)
            )),
        }
    };
    (|| -> Result<(), String> {
        let mut status = 0;
        let Some(mut listener) =
            receive(&parent_end).map_err(|e| format!("receiving the listener: {e}"))?
        else {
            // Nothing sent: the install failed, and the exit code says why.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            exit_code(status)?;
            return Err("the child sent no listener".to_string());
        };
        // The one `getppid`, then the exit: wait for either.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            listener.service();
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid {
                break;
            }
            if std::time::Instant::now() > deadline {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                return Err("the child hung on its notification".to_string());
            }
            let mut pfd = libc::pollfd {
                fd: listener.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut pfd, 1, 10) };
        }
        exit_code(status)?;
        let log = listener.into_log();
        if log.counts.get(&Sysno::getppid).copied().unwrap_or(0) != 1 {
            return Err(format!("expected one getppid, heard {:?}", log.counts));
        }
        Ok(())
    })()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_has_a_compare_and_return_per_watched_syscall() {
        let f = Filter::watching(&[Sysno::socket, Sysno::connect, Sysno::socket]).unwrap();
        // 4 of preamble, 2 per distinct number, 1 final allow.
        assert_eq!(f.len(), 4 + 2 * 2 + 1);
        assert!(Filter::watching(&[]).is_none());
    }

    #[test]
    fn filter_jumps_never_reach_past_the_next_instruction() {
        let all: Vec<Sysno> = Sysno::iter().collect();
        let f = Filter::watching(&all).unwrap();
        assert!(f.prog.len() < 4096, "{} instructions", f.prog.len());
        for insn in &f.prog {
            assert!(insn.jt <= 1 && insn.jf <= 1);
        }
    }

    /// The spawn path the runner uses, with `execve` itself watched: the
    /// child's exec is the first notification, and the parent has to be
    /// answering while it waits for the exec to go through. Run without an
    /// overlay so it exercises the deadlock wherever the tests run.
    #[test]
    fn an_exec_under_a_filter_watching_execve_goes_through() {
        if let Err(why) = probe() {
            eprintln!("skipping: seccomp user notification unavailable: {why}");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let out = std::fs::File::create(tmp.path().join("out")).unwrap();
        let err = std::fs::File::create(tmp.path().join("err")).unwrap();
        let filter = Filter::watching(&[Sysno::execve, Sysno::getppid]).unwrap();
        let (parent_end, child_end) = socketpair().unwrap();
        let child_fd = child_end.as_raw_fd();
        let mut spawn =
            crate::spawn::Spawn::new("/bin/sh", &["sh", "-c", "true"], out, err).unwrap();
        spawn.hook(move || {
            let _keep = &child_end;
            unsafe { filter.install_and_send(child_fd) }
        });
        let spawning = spawn.start().unwrap();
        let mut listener = receive(&parent_end).unwrap().expect("a listener");
        let fd = listener.poll_fd().unwrap();
        let mut child = spawning
            .wait_exec(&[fd], |_| listener.service())
            .expect("the exec went through");
        loop {
            listener.service();
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let log = listener.into_log();
        assert!(
            log.counts.get(&Sysno::execve).is_some_and(|&n| n >= 1),
            "{log:?}"
        );
    }

    /// The round trip on this kernel, when it has it. The probe is also what
    /// the run uses to decide whether to install anything.
    #[test]
    fn probe_round_trip() {
        if let Err(why) = probe() {
            eprintln!("skipping: seccomp user notification unavailable: {why}");
            return;
        }
        let filter = Filter::watching(&[Sysno::getpid, Sysno::getppid]).unwrap();
        let (parent_end, child_end) = socketpair().unwrap();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                if filter.install_and_send(child_end.as_raw_fd()).is_err() {
                    libc::_exit(2);
                }
                libc::getpid();
                libc::getpid();
                libc::getppid();
                libc::_exit(0);
            }
        }
        drop(child_end);
        let mut listener = receive(&parent_end).unwrap().unwrap();
        let mut status = 0;
        loop {
            listener.service();
            if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        let log = listener.into_log();
        assert_eq!(log.counts.get(&Sysno::getpid), Some(&2));
        assert_eq!(log.counts.get(&Sysno::getppid), Some(&1));
    }
}

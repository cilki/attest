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
//! The listener is created in the child, by the `seccomp(2)` call that
//! installs the filter, and attest needs it in the parent. It cannot be
//! *sent* there: from that call on, any syscall the child makes may be one
//! the filter watches (an `in` set watches all but a few), and a watched
//! syscall waits for an answer that only the listener's holder can give — a
//! `sendmsg` carrying the listener would wait on itself. So the child
//! publishes the descriptor's number in memory it shares with the parent and
//! spins, making no syscall at all, while the parent copies the descriptor
//! out of it with `pidfd_getfd(2)` ([`Handoff`]).
//!
//! `CONTINUE` needs Linux 5.5 and `pidfd_getfd` 5.6. [`probe`] finds out once
//! per run whether the whole round trip works; where it does not, no filter is
//! installed and the condition is reported as unmeasured rather than quietly
//! passed.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

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

    /// Install this filter on the calling thread and hand the listener the
    /// kernel returns for it to the parent through `end`, returning once the
    /// parent has taken it ([`Handoff::take`]). For the child, between fork
    /// and exec: nothing here allocates, and nothing after the install makes
    /// a syscall — the wait for the parent is a spin on shared memory, since
    /// any syscall could be one the filter holds until the parent, which does
    /// not have the listener yet, answers it.
    ///
    /// The child's own copy of the listener is close-on-exec (the kernel
    /// makes it so), so the exec drops it; the parent's copy keeps it alive.
    ///
    /// # Safety
    /// `end` must come from a [`Handoff`] created before the fork, whose
    /// parent side calls [`Handoff::take`] with this process's pid.
    pub unsafe fn install_and_hand_off(&self, end: HandoffEnd) -> io::Result<()> {
        // SAFETY: the page was mapped before the fork, and the child keeps its
        // copy of the mapping until it execs or exits.
        let page = unsafe { &*end.0 };
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
            let err = io::Error::last_os_error();
            page.slot.store(FAILED, Ordering::Release);
            return Err(err);
        }
        page.slot.store(fd as i32, Ordering::Release);
        while page.taken.load(Ordering::Acquire) == 0 {
            std::hint::spin_loop();
        }
        Ok(())
    }
}

/// `slot` before the child has installed its filter.
const PENDING: i32 = -1;
/// `slot` once the install has failed: there is no listener to take.
const FAILED: i32 = -2;

/// The page a [`Handoff`] shares between the parent and the child.
#[repr(C)]
struct HandoffPage {
    /// [`PENDING`], [`FAILED`], or the listener's descriptor number in the
    /// child.
    slot: AtomicI32,
    /// Non-zero once the parent holds its own copy of the listener.
    taken: AtomicU32,
}

/// How a child's listener reaches its parent: one shared anonymous mapping,
/// made before the fork so both processes see the same [`HandoffPage`]. The
/// child publishes the listener's descriptor number there
/// ([`Filter::install_and_hand_off`]); the parent copies the descriptor out
/// of the child with `pidfd_getfd(2)` and says so ([`Handoff::take`]). See
/// the module documentation for why the listener cannot simply be sent.
pub struct Handoff {
    page: *mut HandoffPage,
}

/// The child's side of a [`Handoff`]: a pointer into the shared page, valid
/// in the child because the fork copied the mapping.
#[derive(Clone, Copy)]
pub struct HandoffEnd(*const HandoffPage);

impl Handoff {
    /// Map the shared page. Call before the fork.
    pub fn new() -> io::Result<Self> {
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<HandoffPage>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if page == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let page = page.cast::<HandoffPage>();
        // SAFETY: a fresh mapping, page-aligned and large enough.
        unsafe {
            page.write(HandoffPage {
                slot: AtomicI32::new(PENDING),
                taken: AtomicU32::new(0),
            })
        };
        Ok(Self { page })
    }

    /// What the child's hook hands to [`Filter::install_and_hand_off`].
    pub fn child_end(&self) -> HandoffEnd {
        HandoffEnd(self.page)
    }

    fn page(&self) -> &HandoffPage {
        // SAFETY: mapped by `new` and unmapped only by `drop`.
        unsafe { &*self.page }
    }

    /// Take the listener the child `pid` installed: wait for it to publish
    /// the descriptor, copy it out with `pidfd_getfd(2)`, and release the
    /// child to go on to its exec.
    ///
    /// `None` when the child ends without a listener — an earlier hook
    /// failed, or the install did — which the exec-status pipe says more
    /// about. On an error the child, which would spin forever waiting for
    /// this, is `SIGKILL`ed; the caller still reaps it.
    pub fn take(&self, pid: libc::pid_t) -> io::Result<Option<Listener>> {
        let taken = self.take_inner(pid);
        if taken.is_err() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        taken
    }

    fn take_inner(&self, pid: libc::pid_t) -> io::Result<Option<Listener>> {
        let page = self.page();
        // A pidfd both names the child for `pidfd_getfd` and, by polling
        // readable once it exits, says when it ended without publishing.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if pidfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as libc::c_int) };
        loop {
            match page.slot.load(Ordering::Acquire) {
                FAILED => return Ok(None),
                PENDING => {
                    let mut pfd = libc::pollfd {
                        fd: pidfd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // The install takes microseconds; a millisecond's wait
                    // keeps this from spinning beside a child that does.
                    if unsafe { libc::poll(&mut pfd, 1, 1) } < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(err);
                    }
                    // Exited: unless it published on its way, nothing comes.
                    if pfd.revents != 0 && page.slot.load(Ordering::Acquire) == PENDING {
                        return Ok(None);
                    }
                }
                child_fd => {
                    // The copy is close-on-exec, as `pidfd_getfd` makes it.
                    let fd = unsafe {
                        libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), child_fd, 0)
                    };
                    if fd < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    page.taken.store(1, Ordering::Release);
                    return Ok(Some(Listener {
                        fd: unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) },
                        log: SyscallLog::default(),
                        dead: false,
                    }));
                }
            }
        }
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.page.cast(), std::mem::size_of::<HandoffPage>());
        }
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

/// Does the whole round trip work here — install, notify, `CONTINUE`?
///
/// A child installs a filter watching `getppid`, calls it, and exits 0 only
/// if the call still returned its parent's pid; the parent answers the one
/// notification. Anything short of that — `seccomp(2)` refusing the
/// listener flag, `pidfd_getfd` missing (Linux before 5.6) or refused (a
/// Yama `ptrace_scope` that bars it), `CONTINUE` refused (Linux before 5.5),
/// the child never reporting — is the error, which says which.
pub fn probe() -> Result<(), String> {
    let Some(filter) = Filter::watching(&[Sysno::getppid]) else {
        return Err("no filter for this architecture".to_string());
    };
    let handoff = Handoff::new().map_err(|e| format!("mapping the handoff page: {e}"))?;
    let end = handoff.child_end();
    let me = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            let code = match filter.install_and_hand_off(end) {
                Ok(()) => i32::from(libc::getppid() != me),
                Err(e) => 100 + e.raw_os_error().unwrap_or(0).clamp(0, 100),
            };
            libc::_exit(code);
        }
    }
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
        let taken = handoff.take(pid);
        let Some(mut listener) = taken.map_err(|e| {
            // `take` killed the child, which is still ours to reap.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            format!("taking the listener (pidfd_getfd): {e}")
        })?
        else {
            // Nothing published: the install failed, and the exit code says why.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            exit_code(status)?;
            return Err("the child installed no listener".to_string());
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

    /// Spawn `sh -c true` the way the runner does, under a filter watching
    /// `watched`, and return what the listener heard. The parent has to be
    /// answering while it waits for the exec to go through: the exec may be
    /// a watched syscall itself.
    fn exec_under(watched: &[Sysno]) -> SyscallLog {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = std::fs::File::create(tmp.path().join("out")).unwrap();
        let err = std::fs::File::create(tmp.path().join("err")).unwrap();
        let filter = Filter::watching(watched).unwrap();
        let handoff = Handoff::new().unwrap();
        let end = handoff.child_end();
        let mut spawn =
            crate::spawn::Spawn::new("/bin/sh", &["sh", "-c", "true"], out, err).unwrap();
        spawn.hook(move || unsafe { filter.install_and_hand_off(end) });
        let spawning = spawn.start().unwrap();
        let mut listener = handoff.take(spawning.pid()).unwrap().expect("a listener");
        let fd = listener.poll_fd().unwrap();
        let mut child = spawning
            .wait_exec(&[fd], |_| listener.service())
            .expect("the exec went through");
        loop {
            listener.service();
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{status:?}");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        listener.into_log()
    }

    /// The spawn path the runner uses, with `execve` itself watched: the
    /// child's exec is the first notification. Run without an overlay so it
    /// exercises the deadlock wherever the tests run.
    #[test]
    fn an_exec_under_a_filter_watching_execve_goes_through() {
        if let Err(why) = probe() {
            eprintln!("skipping: seccomp user notification unavailable: {why}");
            return;
        }
        let log = exec_under(&[Sysno::execve, Sysno::getppid]);
        assert!(
            log.counts.get(&Sysno::execve).is_some_and(|&n| n >= 1),
            "{log:?}"
        );
    }

    /// Every syscall watched — what an `in` set comes close to: whatever the
    /// child does between the install and the exec is held for an answer,
    /// so handing the listener over must not take a syscall. When it was
    /// sent over a socket, the `sendmsg` carrying it waited on itself.
    #[test]
    fn an_exec_under_a_filter_watching_everything_goes_through() {
        if let Err(why) = probe() {
            eprintln!("skipping: seccomp user notification unavailable: {why}");
            return;
        }
        let all: Vec<Sysno> = Sysno::iter().collect();
        let log = exec_under(&all);
        assert!(
            log.counts.get(&Sysno::execve).is_some_and(|&n| n >= 1),
            "{log:?}"
        );
    }

    /// A child that ends before installing anything (an earlier hook failed)
    /// leaves nothing to take, rather than a parent waiting forever.
    #[test]
    fn take_returns_none_when_the_child_never_publishes() {
        let handoff = Handoff::new().unwrap();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe { libc::_exit(3) };
        }
        assert!(handoff.take(pid).unwrap().is_none());
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 3);
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
        let handoff = Handoff::new().unwrap();
        let end = handoff.child_end();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                if filter.install_and_hand_off(end).is_err() {
                    libc::_exit(2);
                }
                libc::getpid();
                libc::getpid();
                libc::getppid();
                libc::_exit(0);
            }
        }
        let mut listener = handoff.take(pid).unwrap().unwrap();
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

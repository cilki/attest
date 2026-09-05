//! Per-test whole-root overlayfs isolation.
//!
//! Each test runs inside a private copy-on-write view of the filesystem: the
//! lower (read-only) layer is `/`, writes land in a per-test upper layer, and
//! the child `pivot_root`s into the merged view. The overlay lives in the
//! child's private mount namespace, so it is torn down automatically when the
//! child exits (even on kill/timeout) and never touches the host.
//!
//! overlayfs does not cross into the lower layer's submounts, so after
//! overlaying `/` we re-establish them inside the new root:
//!
//! * The mount holding the invocation dir (the "project mount") and the
//!   scratch mounts in [`EPHEMERAL_SCRATCH`] each get their own ephemeral
//!   overlay, so writes to them are also discarded.
//! * Every other mount (`/proc`, `/dev`, `/sys`, file binds like
//!   `/etc/resolv.conf`, …) is recursively bind-mounted through **live**:
//!   those paths are shared with the host, and writes to them persist.
//!
//! [`probe_support`] rehearses the full production setup (same option strings,
//! same pivot_root) once per run in a throwaway child; if neither mode works
//! the caller falls back to running without isolation (with a warning). After
//! a successful probe, a per-test setup failure aborts that test's spawn
//! loudly ([`setup_root`] reports the failing step's errno through `pre_exec`)
//! instead of silently running the test against the real filesystem.
//!
//! Mounting overlayfs needs `CAP_SYS_ADMIN`. When attest already has it
//! ([`Mode::Privileged`]) we just unshare a mount namespace. Otherwise we
//! unshare a user namespace too ([`Mode::Userns`]), which grants
//! `CAP_SYS_ADMIN` within it — at the cost of the test seeing itself as uid 0
//! inside that namespace, and overlays needing the `userxattr` option. Either
//! way the child also gets a private UTS namespace, so a root test changing
//! the hostname does not change the host's.

use std::ffi::{CStr, CString, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::anyhow;

/// How a per-test overlay can be mounted in this environment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Have `CAP_SYS_ADMIN`: unshare only a mount namespace (test keeps its uid).
    Privileged,
    /// Unprivileged: unshare a user + mount namespace and map the caller to uid 0
    /// inside it (the test runs as root within its own namespace).
    Userns,
}

/// Scratch mounts that get their own ephemeral overlay (when they are separate
/// mounts), so the common case of tests writing temp files stays isolated.
const EPHEMERAL_SCRATCH: &[&str] = &["/tmp", "/var/tmp"];

/// A mount under `/` to re-establish inside each test's ephemeral root.
#[derive(Clone)]
pub struct Submount {
    pub source: PathBuf,
    /// Directory mountpoints are recreated with `mkdir`; file binds (e.g.
    /// `/etc/resolv.conf`) get an empty file to bind over.
    pub is_dir: bool,
    /// Re-established as an ephemeral overlay (writes discarded) rather than a
    /// live recursive bind (writes shared with the host).
    pub ephemeral: bool,
}

/// Convert a path to a `CString` for use with libc mount/chdir calls.
fn cstr(path: &Path) -> anyhow::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("path contains NUL: {}", path.display()))
}

/// Build the overlay mount option string, adding `userxattr` for unprivileged
/// mounts (a user namespace cannot set `trusted.*` xattrs).
fn overlay_opts(lower: &Path, upper: &Path, work: &Path, mode: Mode) -> anyhow::Result<CString> {
    let userxattr = if mode == Mode::Userns {
        ",userxattr"
    } else {
        ""
    };
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}{userxattr}",
        lower.display(),
        upper.display(),
        work.display()
    );
    CString::new(opts).map_err(|_| anyhow!("overlay options contain NUL"))
}

/// Write `data` to `path` via raw libc (async-signal-safe; no allocation).
unsafe fn write_proc(path: &CStr, data: &[u8]) -> bool {
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
        if fd < 0 {
            return false;
        }
        let n = libc::write(fd, data.as_ptr().cast(), data.len());
        libc::close(fd);
        n == data.len() as isize
    }
}

/// Enter the namespace(s) required for `mode`: a mount namespace for the
/// overlay, plus a UTS namespace so hostname changes made by a (root) test
/// stay private to it. Returns `true` on success. Only async-signal-safe libc
/// calls are used so this is safe after `fork`/in `pre_exec`.
unsafe fn enter_namespace(mode: Mode, uid_map: &CStr, gid_map: &CStr) -> bool {
    unsafe {
        match mode {
            Mode::Privileged => libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWUTS) == 0,
            Mode::Userns => {
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWUTS) != 0
                {
                    return false;
                }
                // setgroups must be denied before writing gid_map (kernel rule);
                // the file is absent on some kernels, so ignore its result.
                write_proc(c"/proc/self/setgroups", b"deny");
                write_proc(c"/proc/self/uid_map", uid_map.to_bytes())
                    && write_proc(c"/proc/self/gid_map", gid_map.to_bytes())
            }
        }
    }
}

/// Make all mounts private so nothing we mount propagates back to the host.
unsafe fn privatize_mounts() -> bool {
    unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) == 0
    }
}

/// Mount an overlay described by `opts` at `merged`.
unsafe fn mount_overlay(merged: &CStr, opts: &CStr) -> bool {
    unsafe {
        libc::mount(
            c"overlay".as_ptr(),
            merged.as_ptr(),
            c"overlay".as_ptr(),
            0,
            opts.as_ptr().cast(),
        ) == 0
    }
}

/// uid/gid map contents (`0 <id> 1`) that map the caller to root inside a new
/// user namespace. Built in the parent so the `pre_exec` closure never allocates.
fn id_maps() -> (CString, CString) {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    (
        CString::new(format!("0 {uid} 1")).unwrap(),
        CString::new(format!("0 {gid} 1")).unwrap(),
    )
}

/// Unescape mountinfo's octal escapes (space `\040`, tab `\011`, newline `\012`,
/// backslash `\134`) byte-wise, so paths containing them — including non-ASCII
/// bytes — round-trip exactly.
fn unescape_octal(field: &[u8]) -> Vec<u8> {
    fn octal3(d: &[u8]) -> Option<u8> {
        let mut v: u32 = 0;
        for &b in d {
            if !b.is_ascii_digit() || b > b'7' {
                return None;
            }
            v = v * 8 + u32::from(b - b'0');
        }
        u8::try_from(v).ok()
    }
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && i + 3 < field.len()
            && let Some(code) = octal3(&field[i + 1..i + 4])
        {
            out.push(code);
            i += 4;
            continue;
        }
        out.push(field[i]);
        i += 1;
    }
    out
}

/// Mount points from `/proc/self/mountinfo` (field 5), in listing order.
fn mount_points() -> Vec<PathBuf> {
    parse_mount_points(&std::fs::read("/proc/self/mountinfo").unwrap_or_default())
}

/// Parse mountinfo content byte-wise (mount points need not be valid UTF-8).
fn parse_mount_points(data: &[u8]) -> Vec<PathBuf> {
    data.split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(4))
        .map(|field| PathBuf::from(OsString::from_vec(unescape_octal(field))))
        .collect()
}

/// The mount holding `dir`: the longest mount point that is an ancestor of (or
/// equal to) `dir`; `/` when the root filesystem itself holds it.
fn mount_of(dir: &Path, mounts: &[PathBuf]) -> PathBuf {
    mounts
        .iter()
        .filter(|m| dir.starts_with(m))
        .max_by_key(|m| m.components().count())
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Plan how each mount under `/` is re-established inside per-test roots: the
/// project mount (holding `invocation_dir`) and [`EPHEMERAL_SCRATCH`] mounts
/// become ephemeral overlays, everything else a live recursive bind. Mounts
/// already carried by a live ancestor's `MS_REC` bind are dropped; children of
/// ephemeral mounts are kept (an overlay does not cross into them). Sorted
/// shallowest-first so parents mount before children.
pub fn compute_submounts(invocation_dir: &Path) -> Vec<Submount> {
    plan_submounts(mount_points(), invocation_dir, |p| {
        std::fs::symlink_metadata(p)
            .map(|m| m.is_dir())
            .unwrap_or(true)
    })
}

fn plan_submounts(
    mounts: Vec<PathBuf>,
    invocation_dir: &Path,
    is_dir: impl Fn(&Path) -> bool,
) -> Vec<Submount> {
    // Later mountinfo entries shadow earlier ones at the same path; keep the last.
    let mut uniq: Vec<PathBuf> = Vec::new();
    for m in mounts {
        if let Some(i) = uniq.iter().position(|u| *u == m) {
            uniq.remove(i);
        }
        uniq.push(m);
    }

    let root = Path::new("/");
    let project = mount_of(invocation_dir, &uniq);
    let ephemeral_roots: Vec<&Path> = std::iter::once(project.as_path())
        .chain(EPHEMERAL_SCRATCH.iter().map(Path::new))
        .filter(|p| *p != root && uniq.iter().any(|m| m == p))
        .collect();

    uniq.sort_by_key(|p| (p.components().count(), p.clone()));

    let mut out: Vec<Submount> = Vec::new();
    for m in uniq {
        if m == root {
            continue;
        }
        let ephemeral = ephemeral_roots.contains(&m.as_path());
        if !ephemeral
            && out
                .iter()
                .any(|k| !k.ephemeral && m.starts_with(&k.source) && m != k.source)
        {
            continue; // already carried by a live ancestor's recursive bind
        }
        let is_dir = ephemeral || is_dir(&m);
        out.push(Submount {
            source: m,
            is_dir,
            ephemeral,
        });
    }
    out
}

/// Join an absolute path onto `base` (e.g. `base=/x/merged`, `abs=/proc` →
/// `/x/merged/proc`).
fn join_abs(base: &Path, abs: &Path) -> PathBuf {
    base.join(abs.strip_prefix("/").unwrap_or(abs))
}

/// Layout of one test's overlay dirs inside its context dir. Owned here so the
/// runner and this module never disagree about names.
pub fn upper_dir(context: &Path) -> PathBuf {
    context.join("upper")
}
fn work_dir(context: &Path) -> PathBuf {
    context.join("work")
}
fn merged_dir(context: &Path) -> PathBuf {
    context.join("merged")
}
/// Upper layer of the i-th ephemeral submount, counting `ephemeral` entries of
/// the submount plan in order.
pub fn submount_upper_dir(context: &Path, i: usize) -> PathBuf {
    context.join(format!("sub{i}-upper"))
}
fn submount_work_dir(context: &Path, i: usize) -> PathBuf {
    context.join(format!("sub{i}-work"))
}

/// One mount action performed inside the child, in order.
enum MountStep {
    /// Ephemeral overlay mounted at `target`.
    Overlay { opts: CString, target: CString },
    /// Live recursive bind of `src` at `dst`.
    Bind {
        src: CString,
        dst: CString,
        is_dir: bool,
    },
}

/// A precomputed plan for building one test's ephemeral root. Everything is
/// prepared (and all directories created) in the parent so the post-fork
/// `pre_exec` closure ([`setup_root`]) only makes async-signal-safe libc calls
/// with no allocation.
pub struct RootOverlay {
    mode: Mode,
    uid_map: CString,
    gid_map: CString,
    /// Overlay opts for `/`: `lowerdir=/,upperdir=..,workdir=..[,userxattr]`.
    root_opts: CString,
    /// Mount point of the `/` overlay; also the pivot_root new-root.
    merged: CString,
    /// Submount overlays and live binds, shallowest-first.
    steps: Vec<MountStep>,
    /// `(source, target)` bind keeping attest's own context dir live inside the
    /// new root, so `functions.sh` and log capture work even though the context
    /// falls under an overlaid mount (it usually lives under `/tmp`).
    context: (CString, CString),
    /// `put_old` directory for pivot_root (under `merged`).
    put_old: CString,
    /// Absolute invocation dir to `chdir` into after pivoting.
    chdir_to: CString,
}

impl RootOverlay {
    /// Build the plan for one test and create its overlay dirs under `context`.
    /// `submounts` is the shared plan from [`compute_submounts`].
    pub fn build(
        mode: Mode,
        context: &Path,
        invocation_dir: &Path,
        submounts: &[Submount],
    ) -> anyhow::Result<Self> {
        let merged = merged_dir(context);
        let (upper, work) = (upper_dir(context), work_dir(context));
        for dir in [&upper, &work, &merged] {
            std::fs::create_dir_all(dir)?;
        }
        let (uid_map, gid_map) = id_maps();

        let mut steps = Vec::with_capacity(submounts.len());
        let mut eph = 0;
        for sm in submounts {
            let target = cstr(&join_abs(&merged, &sm.source))?;
            if sm.ephemeral {
                let (sub_upper, sub_work) = (
                    submount_upper_dir(context, eph),
                    submount_work_dir(context, eph),
                );
                eph += 1;
                std::fs::create_dir_all(&sub_upper)?;
                std::fs::create_dir_all(&sub_work)?;
                steps.push(MountStep::Overlay {
                    opts: overlay_opts(&sm.source, &sub_upper, &sub_work, mode)?,
                    target,
                });
            } else {
                steps.push(MountStep::Bind {
                    src: cstr(&sm.source)?,
                    dst: target,
                    is_dir: sm.is_dir,
                });
            }
        }

        Ok(RootOverlay {
            mode,
            uid_map,
            gid_map,
            root_opts: overlay_opts(Path::new("/"), &upper, &work, mode)?,
            put_old: cstr(&merged.join("oldroot"))?,
            context: (cstr(context)?, cstr(&join_abs(&merged, context))?),
            merged: cstr(&merged)?,
            steps,
            chdir_to: cstr(invocation_dir)?,
        })
    }
}

/// Register a `pre_exec` hook on `cmd` that builds the per-test ephemeral root.
/// A setup failure aborts the exec and surfaces as a spawn error (carrying the
/// failing step's errno), so a test never silently runs against the real
/// filesystem believing it is isolated.
pub fn register_root_mount(cmd: &mut Command, plan: RootOverlay) {
    unsafe {
        cmd.pre_exec(move || setup_root(&plan));
    }
}

/// Construct the ephemeral root inside the forked child. Only async-signal-safe
/// libc calls; all strings were built in the parent. Errors carry the failing
/// step's errno — the only detail that survives the `pre_exec` pipe.
unsafe fn setup_root(p: &RootOverlay) -> std::io::Result<()> {
    fn fail() -> std::io::Result<()> {
        Err(std::io::Error::last_os_error())
    }
    unsafe {
        // Namespaces (mount ns; + user ns when unprivileged) and stop propagation.
        if !enter_namespace(p.mode, &p.uid_map, &p.gid_map) || !privatize_mounts() {
            return fail();
        }
        // Ephemeral overlay over the whole root.
        if !mount_overlay(&p.merged, &p.root_opts) {
            return fail();
        }
        for step in &p.steps {
            match step {
                // Isolation-critical: a missing submount overlay would leave its
                // writes going to the host, so failure is fatal.
                MountStep::Overlay { opts, target } => {
                    libc::mkdir(target.as_ptr(), 0o755);
                    if !mount_overlay(target, opts) {
                        return fail();
                    }
                }
                // Best-effort: a live bind that fails only limits what the test
                // can see at this path — writes there still land in the root
                // overlay's upper layer, so isolation is not weakened. O_EXCL
                // avoids copying up bind targets that already exist in the
                // lower layer (only a missing target needs creating).
                MountStep::Bind { src, dst, is_dir } => {
                    if *is_dir {
                        libc::mkdir(dst.as_ptr(), 0o755);
                    } else {
                        let fd = libc::open(
                            dst.as_ptr(),
                            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                            0o644,
                        );
                        if fd >= 0 {
                            libc::close(fd);
                        }
                    }
                    libc::mount(
                        src.as_ptr(),
                        dst.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REC,
                        std::ptr::null(),
                    );
                }
            }
        }
        // Keep attest's own context dir live (functions.sh + log capture); the
        // run is broken without it, so failure is fatal.
        let (ctx_src, ctx_dst) = &p.context;
        libc::mkdir(ctx_dst.as_ptr(), 0o755);
        if libc::mount(
            ctx_src.as_ptr(),
            ctx_dst.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        ) != 0
        {
            return fail();
        }
        // Switch into the new root, then drop the old one.
        libc::mkdir(p.put_old.as_ptr(), 0o755);
        if libc::chdir(p.merged.as_ptr()) != 0
            || libc::syscall(libc::SYS_pivot_root, p.merged.as_ptr(), p.put_old.as_ptr()) != 0
        {
            return fail();
        }
        libc::chdir(c"/".as_ptr());
        libc::umount2(c"/oldroot".as_ptr(), libc::MNT_DETACH);
        libc::rmdir(c"/oldroot".as_ptr());
        // Land in the (now ephemeral) invocation directory; the runner script
        // then cds into the per-test working directory.
        if libc::chdir(p.chdir_to.as_ptr()) != 0 {
            return fail();
        }
    }
    Ok(())
}

/// Rehearse the full production setup (same option strings, same pivot_root)
/// with `mode`, in a forked child so the caller's namespaces are never touched.
fn probe_mode(scratch: &Path, mode: Mode, invocation_dir: &Path, submounts: &[Submount]) -> bool {
    let context = scratch.join(match mode {
        Mode::Privileged => ".attest-probe-priv",
        Mode::Userns => ".attest-probe-userns",
    });
    let _ = std::fs::remove_dir_all(&context);
    let ok = match RootOverlay::build(mode, &context, invocation_dir, submounts) {
        Ok(plan) => unsafe {
            let pid = libc::fork();
            if pid < 0 {
                false
            } else if pid == 0 {
                libc::_exit(i32::from(setup_root(&plan).is_err()));
            } else {
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
            }
        },
        Err(_) => false,
    };
    let _ = std::fs::remove_dir_all(&context);
    ok
}

/// Probe how (if at all) per-test roots can be built, with context dirs created
/// under `scratch_parent` (the same filesystem that will host the real per-test
/// contexts). Performed once per run. Prefers the privileged mode so tests keep
/// their real uid when attest has `CAP_SYS_ADMIN`.
pub fn probe_support(
    scratch_parent: &Path,
    invocation_dir: &Path,
    submounts: &[Submount],
) -> Option<Mode> {
    [Mode::Privileged, Mode::Userns]
        .into_iter()
        .find(|&mode| probe_mode(scratch_parent, mode, invocation_dir, submounts))
}

/// Recursively copy `src` into `dst`, creating `dst` if needed. Regular files,
/// directories, and symlinks are reproduced; other special files (e.g. overlay
/// whiteout device nodes for deletions) are skipped.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(target, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_handles_known_escapes() {
        assert_eq!(unescape_octal(b"/mnt/a\\040b"), b"/mnt/a b");
        assert_eq!(unescape_octal(b"/tab\\011x"), b"/tab\tx");
        assert_eq!(unescape_octal(b"/back\\134slash"), b"/back\\slash");
        assert_eq!(unescape_octal(b"/plain"), b"/plain");
    }

    #[test]
    fn unescape_preserves_non_ascii_bytes() {
        // Raw multibyte UTF-8 passes through untouched...
        assert_eq!(
            unescape_octal("/na\u{ef}ve".as_bytes()),
            "/na\u{ef}ve".as_bytes()
        );
        // ...and escaped high bytes decode to the byte, not a mangled char.
        assert_eq!(unescape_octal(b"/x\\303\\257y"), b"/x\xc3\xafy");
    }

    #[test]
    fn unescape_leaves_invalid_escapes_alone() {
        assert_eq!(unescape_octal(b"/a\\9zz"), b"/a\\9zz");
        assert_eq!(unescape_octal(b"/a\\777"), b"/a\\777"); // > 255
        assert_eq!(unescape_octal(b"/trunc\\04"), b"/trunc\\04");
    }

    #[test]
    fn parse_mountinfo_extracts_mount_points() {
        let data = b"36 35 98:0 / / rw shared:1 - ext4 /dev/sda rw\n\
                     37 36 0:5 / /proc rw - proc proc rw\n\
                     38 36 0:6 / /mnt/a\\040b rw - tmpfs tmpfs rw\n";
        let points = parse_mount_points(data);
        assert_eq!(
            points,
            vec![
                PathBuf::from("/"),
                PathBuf::from("/proc"),
                PathBuf::from("/mnt/a b")
            ]
        );
    }

    #[test]
    fn plan_marks_project_and_scratch_ephemeral() {
        let mounts = ["/", "/proc", "/tmp", "/workspace"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let plan = plan_submounts(mounts, Path::new("/workspace/proj"), |_| true);
        let get = |p: &str| plan.iter().find(|s| s.source == Path::new(p)).unwrap();
        assert!(!plan.iter().any(|s| s.source == Path::new("/")));
        assert!(!get("/proc").ephemeral);
        assert!(get("/tmp").ephemeral);
        assert!(get("/workspace").ephemeral);
    }

    #[test]
    fn plan_prunes_children_of_live_mounts_only() {
        let mounts = [
            "/",
            "/proc",
            "/proc/sys/fs/binfmt_misc", // carried by /proc's recursive bind
            "/workspace",
            "/workspace/nested", // under an overlay: must be re-established
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let plan = plan_submounts(mounts, Path::new("/workspace"), |_| true);
        let paths: Vec<&Path> = plan.iter().map(|s| s.source.as_path()).collect();
        assert!(!paths.contains(&Path::new("/proc/sys/fs/binfmt_misc")));
        assert!(paths.contains(&Path::new("/workspace/nested")));
        assert!(
            !plan
                .iter()
                .find(|s| s.source == Path::new("/workspace/nested"))
                .unwrap()
                .ephemeral
        );
    }

    #[test]
    fn plan_sorts_parents_first_and_dedups() {
        let mounts = ["/", "/a/b", "/a", "/tmp", "/a", "/etc/resolv.conf"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let plan = plan_submounts(mounts, Path::new("/x"), |p| {
            p != Path::new("/etc/resolv.conf")
        });
        // /a appears once, before /a/b would (which is pruned as a live child).
        assert_eq!(
            plan.iter().filter(|s| s.source == Path::new("/a")).count(),
            1
        );
        for pair in plan.windows(2) {
            assert!(
                pair[0].source.components().count() <= pair[1].source.components().count(),
                "not sorted shallowest-first"
            );
        }
        let resolv = plan
            .iter()
            .find(|s| s.source == Path::new("/etc/resolv.conf"))
            .unwrap();
        assert!(!resolv.is_dir);
    }

    #[test]
    fn copy_dir_reproduces_files_dirs_and_symlinks() {
        let src = tempfile::TempDir::new().unwrap();
        let dst = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/f.txt"), "hi").unwrap();
        std::os::unix::fs::symlink("sub/f.txt", src.path().join("link")).unwrap();

        copy_dir_recursive(src.path(), dst.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.path().join("sub/f.txt")).unwrap(),
            "hi"
        );
        let link = dst.path().join("link");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            PathBuf::from("sub/f.txt")
        );
    }
}

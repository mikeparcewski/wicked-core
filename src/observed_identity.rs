//! (core#1543) Seat identity OBSERVED at runtime, never predicted from configuration.
//!
//! Five bypasses of the predicted identity (a frozen invocation, a quoted wrapper, a quoted-path
//! alias, one binary under two names, a relative-path alias) all came from guessing what a launch
//! runs from its strings and paths. Here the launcher looks at the processes it actually started:
//!
//! - A process's IDENTITY is the content hash of the executable image the kernel runs for it, read
//!   from the live process (`/proc/<pid>/exe` on Linux, `proc_pidpath` on macOS). For an
//!   interpreter image (node, python, a shell…) it is the interpreter's hash plus the hash of the
//!   script it runs (from the process's own argv, resolved against its own cwd), or of its inline
//!   program text (`sh -c …`). Names, paths, quoting and the working directory play no part, so one
//!   binary under any name or path is one identity, and two different files are two.
//! - The launch's PRINCIPALS are the processes under the launched pid that hold the launch's own
//!   stdout pipe and have no descendant holding it: what the launched CLI turned into, through any
//!   wrapper (a sandbox that execs or forks it, a script that execs or spawns it, a `node` shim and
//!   its native binary). The agent's own tool calls run on pipes it captures, so a `git` or `cargo`
//!   both seats run is never a principal. A principal counts once it was seen in two samples, so a
//!   launcher caught for an instant before it spawned the CLI is not one.
//! - Anything that cannot be observed (Windows, a seat pod, a read that failed, an interpreter whose
//!   script cannot be resolved) makes the launch UNOBSERVED. Unobserved never counts as distinct:
//!   callers fail closed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An observed identity: `sha256:<hex>` for a native image, `sha256:<interp>+sha256:<script>` or
/// `sha256:<interp>+inline:<sha256 of the text>` for an interpreter.
pub(crate) type Identity = String;

/// Interpreter images whose identity is the program they run, not themselves (basename,
/// case-folded, version suffixes such as `python3.12` folded to `python`).
fn interpreter(exe: &Path) -> Option<&'static str> {
    let name = exe.file_name()?.to_str()?.to_ascii_lowercase();
    let stem = name.trim_end_matches(".exe");
    let base = stem.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    [
        "node", "nodejs", "python", "bun", "deno", "ruby", "perl", "sh", "bash", "zsh", "dash",
        "env",
    ]
    .into_iter()
    .find(|i| *i == base)
}

/// sha256 of a file's content, cached by (device, inode, size, mtime): a CLI binary is hashed once
/// per process however many samples see it.
fn hash_file(f: &mut std::fs::File) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    use std::sync::{Mutex, OnceLock};
    type Key = (u64, u64, u64, i128);
    static CACHE: OnceLock<Mutex<HashMap<Key, String>>> = OnceLock::new();
    let m = f.metadata()?;
    let key = file_key(&m);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(h) = cache.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return Ok(h.clone());
    }
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let out = format!("sha256:{:x}", h.finalize());
    cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, out.clone());
    Ok(out)
}

#[cfg(unix)]
fn file_key(m: &std::fs::Metadata) -> (u64, u64, u64, i128) {
    use std::os::unix::fs::MetadataExt;
    (
        m.dev(),
        m.ino(),
        m.size(),
        i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
    )
}

#[cfg(not(unix))]
fn file_key(m: &std::fs::Metadata) -> (u64, u64, u64, i128) {
    (0, 0, m.len(), 0)
}

/// The identity of an interpreter process from its argv (pure, given a hasher): the first argument
/// that is not an option is the script, resolved against the process's own `cwd`; `-c`/`-e` (and
/// node's `--eval`) take the next argument as inline program text. `None`: no program could be
/// established (unobserved).
fn interpreter_identity(
    interp_hash: &str,
    kind: &str,
    argv: &[String],
    cwd: Option<&Path>,
    hash_path: &dyn Fn(&Path) -> Option<String>,
) -> Option<Identity> {
    use sha2::Digest;
    // The options that take the program as their next argument, per interpreter.
    let inline: &[&str] = match kind {
        "node" | "nodejs" | "bun" | "deno" => &["-e", "--eval", "-p", "--print"],
        "python" => &["-c", "-m"],
        "ruby" | "perl" => &["-e"],
        "env" => &[],
        _ => &["-c"],
    };
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        if inline.contains(&a.as_str()) {
            let text = args.next()?;
            return Some(format!(
                "{interp_hash}+inline:{:x}",
                sha2::Sha256::digest(text.as_bytes())
            ));
        }
        if a.starts_with('-') {
            continue;
        }
        if kind == "env" {
            // `env VAR=1 prog …` execs `prog` in place: the next sample sees it.
            return None;
        }
        let p = Path::new(a);
        let p: PathBuf = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd?.join(p)
        };
        return hash_path(&p).map(|s| format!("{interp_hash}+{s}"));
    }
    // An interactive interpreter with no program: its own image (a bare shell reading stdin).
    Some(interp_hash.to_string())
}

/// What one process is: its identity, or `None` when it could not be read (unobserved).
fn identity_of(pid: i32) -> Option<Identity> {
    let (exe_path, mut exe) = os::open_exe(pid)?;
    let exe_hash = hash_file(&mut exe).ok()?;
    match interpreter(&exe_path) {
        None => Some(exe_hash),
        Some(kind) => {
            let argv = os::argv(pid)?;
            let cwd = os::cwd(pid);
            let hash_path = |p: &Path| {
                std::fs::File::open(p)
                    .ok()
                    .and_then(|mut f| hash_file(&mut f).ok())
            };
            interpreter_identity(&exe_hash, kind, &argv, cwd.as_deref(), &hash_path)
        }
    }
}

/// Watches one launch: call [`Observer::sample`] while it runs, [`Observer::finish`] at its end.
pub(crate) struct Observer {
    root: i32,
    pipe: os::PipeKey,
    /// Per identity, how many samples saw it as a principal.
    counts: HashMap<Identity, u32>,
    /// A sample that could not read something it needed: the launch is unobserved.
    failed: bool,
    /// Per principal pid, how many samples could not establish its identity while it lived: one
    /// is a process caught mid-fork or mid-exit; two make the launch unobserved.
    unreadable: HashMap<i32, u32>,
    samples: u32,
}

impl Observer {
    /// Watch the launch whose process is `root`, writing to the pipe whose READ end the launcher
    /// holds as `stdout_fd`. `None` when this platform cannot observe (the launch is unobserved).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn new(root: u32, stdout_fd: i32) -> Option<Self> {
        let pipe = os::pipe_key(stdout_fd)?;
        Some(Self {
            root: i32::try_from(root).ok()?,
            pipe,
            counts: HashMap::new(),
            failed: false,
            unreadable: HashMap::new(),
            samples: 0,
        })
    }

    /// One look at the launch's tree: its principals (processes holding the launch's stdout with no
    /// descendant holding it) and their identities.
    pub(crate) fn sample(&mut self) {
        let Some(tree) = os::descendants(self.root) else {
            // The launch ended between two looks: nothing more to see. A listing that fails while
            // it runs makes it unobserved.
            if os::alive(self.root) {
                self.failed = true;
            }
            return;
        };
        let holders: Vec<i32> = tree
            .iter()
            .map(|(pid, _)| *pid)
            .filter(|pid| os::holds_pipe(*pid, &self.pipe))
            .collect();
        if holders.is_empty() {
            return; // exited between the listing and the look, or not started yet
        }
        self.samples += 1;
        for pid in holders.iter().copied().filter(|p| {
            // A leaf principal: no other holder below it.
            !holders.iter().any(|q| q != p && is_ancestor(&tree, *p, *q))
        }) {
            match identity_of(pid) {
                Some(id) => *self.counts.entry(id).or_default() += 1,
                None => {
                    // `env` about to exec, or a process caught mid-fork or mid-exit: a failure
                    // only when the same live process stays unreadable.
                    if os::alive(pid) && !os::is_env(pid) {
                        let n = self.unreadable.entry(pid).or_default();
                        *n += 1;
                        if *n >= 2 {
                            self.failed = true;
                        }
                    }
                }
            }
        }
    }

    /// The launch's principal identities, each seen in two samples or more; `None` when the
    /// launch is unobserved (a failed read, or no principal ever held for two samples).
    pub(crate) fn finish(self) -> Option<Vec<Identity>> {
        if self.failed {
            return None;
        }
        let mut ids: Vec<Identity> = self
            .counts
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        (!ids.is_empty()).then_some(ids)
    }
}

/// Whether `a` is an ancestor of `b` in `tree` (pid -> parent pid).
fn is_ancestor(tree: &[(i32, i32)], a: i32, b: i32) -> bool {
    let parent: HashMap<i32, i32> = tree.iter().copied().collect();
    let mut cur = b;
    for _ in 0..64 {
        match parent.get(&cur) {
            Some(&p) if p == a => return true,
            Some(&p) => cur = p,
            None => return false,
        }
    }
    false
}

/// What one launch was observed to run: the seat it was launched as and its principal identities
/// (`None`: unobserved — a seat pod, Windows, a failed read, a carrier that does not observe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchReceipt {
    pub(crate) seat: String,
    pub(crate) observed: Option<Vec<Identity>>,
}

thread_local! {
    /// The receipts of the launches this thread made, in order: the carrier runs each launch on
    /// the caller's thread, so the engine reads back exactly the launches it asked for
    /// ([`take_receipts`]). A carrier that never records leaves the launch unobserved.
    static RECEIPTS: std::cell::RefCell<Vec<LaunchReceipt>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The carrier's half: record what a launch it ran on this thread was observed to run.
pub(crate) fn record_receipt(seat: &str, observed: Option<Vec<Identity>>) {
    RECEIPTS.with(|r| {
        r.borrow_mut().push(LaunchReceipt {
            seat: seat.to_string(),
            observed,
        })
    });
}

/// The engine's half: every receipt recorded on this thread since the last take.
pub(crate) fn take_receipts() -> Vec<LaunchReceipt> {
    RECEIPTS.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

/// Everything the launches of `seat` among `receipts` were observed to run; `None` when there was
/// none, or any of them was unobserved (one unobserved turn hides what it ran).
pub(crate) fn observed_for(receipts: &[LaunchReceipt], seat: &str) -> Option<Vec<Identity>> {
    let mut out: Vec<Identity> = Vec::new();
    let mut any = false;
    for r in receipts.iter().filter(|r| r.seat == seat) {
        any = true;
        for id in r.observed.as_ref()? {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
    }
    any.then_some(out)
}

/// (core#1543) Whether a judge's verdict stands against what the work was OBSERVED to run: a
/// judge observed running one of the work's identities judged its own work, so the verdict is
/// discarded and the reason returned. Anything unobserved proves nothing either way (`None`):
/// the caller's own rules stand.
pub(crate) fn judged_own_work(
    judge: Option<&[Identity]>,
    work: Option<&[Identity]>,
) -> Option<String> {
    let (judge, work) = (judge?, work?);
    meets(judge, work).then(|| {
        "the judge was observed running the same program as the work it judged (a self-grade):          its verdict is discarded"
            .to_string()
    })
}

/// Whether two observed identity sets share one: `false` when either is unobserved (`None`) —
/// the caller decides what unobserved means, always failing closed.
pub(crate) fn meets(a: &[Identity], b: &[Identity]) -> bool {
    a.iter().any(|x| b.contains(x))
}

#[cfg(target_os = "linux")]
mod os {
    use std::path::{Path, PathBuf};

    pub(crate) struct PipeKey(u64);

    pub(crate) fn pipe_key(fd: i32) -> Option<PipeKey> {
        // SAFETY: zeroed `stat` is a valid out-parameter; `fd` is the launcher's open read end.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(PipeKey(st.st_ino))
    }

    pub(crate) fn holds_pipe(pid: i32, key: &PipeKey) -> bool {
        std::fs::read_link(format!("/proc/{pid}/fd/1"))
            .ok()
            .and_then(|l| l.to_str().map(str::to_string))
            .is_some_and(|l| l == format!("pipe:[{}]", key.0))
    }

    /// `root` and every process below it, as (pid, parent pid).
    pub(crate) fn descendants(root: i32) -> Option<Vec<(i32, i32)>> {
        let mut all: Vec<(i32, i32)> = Vec::new();
        for e in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // `pid (comm) state ppid …`: comm may hold spaces and parens; the last `)` ends it.
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                continue;
            };
            let ppid = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok());
            if let Some(ppid) = ppid {
                all.push((pid, ppid));
            }
        }
        let mut out = vec![(root, 0)];
        let mut i = 0;
        while i < out.len() {
            let p = out[i].0;
            out.extend(all.iter().copied().filter(|(_, pp)| *pp == p));
            i += 1;
        }
        all.iter().any(|(p, _)| *p == root).then_some(out)
    }

    pub(crate) fn open_exe(pid: i32) -> Option<(PathBuf, std::fs::File)> {
        let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        // Opened THROUGH /proc: the image the process runs, even if its path was replaced since.
        let f = std::fs::File::open(format!("/proc/{pid}/exe")).ok()?;
        Some((path, f))
    }

    pub(crate) fn argv(pid: i32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(
            raw.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect(),
        )
    }

    pub(crate) fn cwd(pid: i32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    pub(crate) fn alive(pid: i32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    pub(crate) fn is_env(pid: i32) -> bool {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n == "env"))
            .unwrap_or(false)
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::path::PathBuf;

    /// `struct proc_fileinfo` / `pipe_info` / `pipe_fdinfo` (sys/proc_info.h), which `libc` does
    /// not declare.
    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }
    #[repr(C)]
    struct PipeInfo {
        pipe_stat: libc::vinfo_stat,
        pipe_handle: u64,
        pipe_peerhandle: u64,
        pipe_status: i32,
        rfu_1: i32,
    }
    #[repr(C)]
    struct PipeFdInfo {
        pfi: ProcFileInfo,
        pipeinfo: PipeInfo,
    }
    const PROC_PIDFDPIPEINFO: i32 = 6;

    /// Our read end's (handle, peer handle).
    pub(crate) struct PipeKey(u64, u64);

    fn pipe_of(pid: i32, fd: i32) -> Option<(u64, u64)> {
        // SAFETY: zeroed POD out-parameter of the declared size.
        let mut info: PipeFdInfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<PipeFdInfo>() as i32;
        let n = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDPIPEINFO,
                (&mut info as *mut PipeFdInfo).cast(),
                size,
            )
        };
        (n == size).then_some((info.pipeinfo.pipe_handle, info.pipeinfo.pipe_peerhandle))
    }

    pub(crate) fn pipe_key(fd: i32) -> Option<PipeKey> {
        let (h, p) = pipe_of(std::process::id() as i32, fd)?;
        Some(PipeKey(h, p))
    }

    pub(crate) fn holds_pipe(pid: i32, key: &PipeKey) -> bool {
        pipe_of(pid, 1).is_some_and(|(h, p)| h == key.1 || h == key.0 || p == key.0)
    }

    fn children(pid: i32) -> Option<Vec<i32>> {
        let mut buf = vec![0i32; 4096];
        let n = unsafe {
            libc::proc_listchildpids(
                pid,
                buf.as_mut_ptr().cast(),
                (buf.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if n < 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(buf.into_iter().filter(|p| *p > 0).collect())
    }

    pub(crate) fn descendants(root: i32) -> Option<Vec<(i32, i32)>> {
        if !alive(root) {
            return None;
        }
        let mut out = vec![(root, 0)];
        let mut i = 0;
        while i < out.len() {
            let p = out[i].0;
            for c in children(p)? {
                out.push((c, p));
            }
            i += 1;
        }
        Some(out)
    }

    pub(crate) fn exe_path(pid: i32) -> Option<PathBuf> {
        let mut buf = vec![0u8; 4096];
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
    }

    pub(crate) fn open_exe(pid: i32) -> Option<(PathBuf, std::fs::File)> {
        let path = exe_path(pid)?;
        let f = std::fs::File::open(&path).ok()?;
        Some((path, f))
    }

    /// The process's argv, from `KERN_PROCARGS2` (argc, the exec path, padding, then argv).
    pub(crate) fn argv(pid: i32) -> Option<Vec<String>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size < 4 {
            return None;
        }
        let mut buf = vec![0u8; size];
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        buf.truncate(size);
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
        let rest = &buf[4..];
        // Skip the exec path, then the NUL padding.
        let mut i = rest.iter().position(|b| *b == 0)?;
        while i < rest.len() && rest[i] == 0 {
            i += 1;
        }
        let args: Vec<String> = rest[i..]
            .split(|b| *b == 0)
            .take(argc)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        (args.len() == argc).then_some(args)
    }

    pub(crate) fn cwd(pid: i32) -> Option<PathBuf> {
        // SAFETY: zeroed POD out-parameter of the declared size.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if n != size {
            return None;
        }
        // `vip_path` is `char[MAXPATHLEN]`, which `libc` declares as rows of 32.
        let bytes: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        (!bytes.is_empty()).then(|| PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub(crate) fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    pub(crate) fn is_env(pid: i32) -> bool {
        exe_path(pid).is_some_and(|p| p.file_name().is_some_and(|n| n == "env"))
    }
}

/// Every other platform (Windows): nothing is observed, so every launch is unobserved and callers
/// fail closed.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod os {
    use std::path::PathBuf;
    pub(crate) struct PipeKey;
    pub(crate) fn pipe_key(_fd: i32) -> Option<PipeKey> {
        None
    }
    pub(crate) fn holds_pipe(_pid: i32, _key: &PipeKey) -> bool {
        false
    }
    pub(crate) fn descendants(_root: i32) -> Option<Vec<(i32, i32)>> {
        None
    }
    pub(crate) fn open_exe(_pid: i32) -> Option<(PathBuf, std::fs::File)> {
        None
    }
    pub(crate) fn argv(_pid: i32) -> Option<Vec<String>> {
        None
    }
    pub(crate) fn cwd(_pid: i32) -> Option<PathBuf> {
        None
    }
    pub(crate) fn alive(_pid: i32) -> bool {
        false
    }
    pub(crate) fn is_env(_pid: i32) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "observed_identity_tests.rs"]
mod tests;

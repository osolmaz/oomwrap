//! A job in its own transient scope: the lock of its guard, its readings,
//! the watch of its `memory.events`, and the stop sequence through
//! `cgroup.kill`.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use nix::sys::signal::Signal;

use super::checks::{ScopeCgroup, is_own_scope_dir};
use super::{ProcessGroup, process_group_exists, signal_process_group};
use crate::host::read_file;
use crate::readings::{
    JobReadings, MemoryEvents, parse_bytes, parse_cgroup_v2_path, parse_memory_events,
    parse_psi_full_avg10,
};
use crate::signal::{ProcessId, signal_process};

/// How long the stop sequence waits for the scope to empty after
/// `cgroup.kill`.
const SCOPE_KILL_WAIT: Duration = Duration::from_secs(2);
const SETTLE_STEP: Duration = Duration::from_millis(20);
/// How often the watch thread looks whether it should end, in milliseconds.
const WATCH_WAKE_MS: u16 = 200;

/// The lock that a guard holds for its whole run. A scope `oomwrap-*` whose
/// lock nobody holds has lost its guard; unlike a process ID in the unit
/// name, a lock does not fail after the ID is reused.
pub(crate) struct ScopeLock {
    path: PathBuf,
    _file: File,
}

impl ScopeLock {
    pub(crate) fn acquire(dir: &Path, unit: &str) -> Result<Self> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("failed to create the lock folder {}", dir.display()))?;
        let path = dir.join(format!("{unit}.lock"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create the lock {}", path.display()))?;
        file.try_lock()
            .map_err(|error| anyhow!("failed to lock {}: {error}", path.display()))?;
        Ok(Self { path, _file: file })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopeLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The highest counters seen so far: each reading only raises them.
fn merge(into: &Mutex<MemoryEvents>, events: MemoryEvents) -> MemoryEvents {
    let mut seen = into.lock().unwrap_or_else(PoisonError::into_inner);
    seen.oom = seen.oom.max(events.oom);
    seen.oom_kill = seen.oom_kill.max(events.oom_kill);
    *seen
}

/// Watches a scope's `memory.events` with inotify. When the kernel kills
/// the job's last process, systemd can remove the scope before the guard
/// sees the exit; the watch reads the counters when they change, so the
/// guard still knows why the job ended.
struct EventsWatch {
    seen: Arc<Mutex<MemoryEvents>>,
    stop: Arc<AtomicBool>,
}

impl EventsWatch {
    fn start(path: PathBuf) -> Result<Self> {
        let inotify = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK)?;
        inotify
            .add_watch(&path, AddWatchFlags::IN_MODIFY)
            .with_context(|| format!("failed to watch {}", path.display()))?;
        let seen = Arc::new(Mutex::new(MemoryEvents::default()));
        let stop = Arc::new(AtomicBool::new(false));
        read_into(&path, &seen);
        let (thread_seen, thread_stop) = (Arc::clone(&seen), Arc::clone(&stop));
        thread::Builder::new()
            .name("memory-events".to_string())
            .spawn(move || watch_events(&inotify, &path, &thread_seen, &thread_stop))
            .context("failed to start the watch of memory.events")?;
        Ok(Self { seen, stop })
    }
}

impl Drop for EventsWatch {
    /// The thread ends within one wake interval; nothing waits for it.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn read_into(path: &Path, seen: &Mutex<MemoryEvents>) {
    if let Some(events) = fs::read_to_string(path)
        .ok()
        .and_then(|text| parse_memory_events(&text).ok())
    {
        merge(seen, events);
    }
}

fn watch_events(inotify: &Inotify, path: &Path, seen: &Mutex<MemoryEvents>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let mut fds = [PollFd::new(inotify.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(WATCH_WAKE_MS)) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(_) => return,
        }
        let Ok(events) = inotify.read_events() else {
            continue;
        };
        read_into(path, seen);
        if events
            .iter()
            .any(|event| event.mask.contains(AddWatchFlags::IN_IGNORED))
        {
            return;
        }
    }
}

/// A job in a transient scope. The leader of its process group is the
/// guard's own child, as in `poll`; the stop sequence reaches every process
/// of the scope, also one that left the group.
pub(crate) struct Scope {
    pub(super) group: ProcessGroup,
    cgroup: ScopeCgroup,
    /// The process that keeps the scope until the guard releases its lock;
    /// it is no member of the job.
    holder: Option<u32>,
    watch: Option<EventsWatch>,
    _lock: ScopeLock,
}

impl Scope {
    pub(crate) fn new(
        group: ProcessGroup,
        cgroup: ScopeCgroup,
        holder: Option<u32>,
        lock: ScopeLock,
    ) -> Self {
        let watch = match EventsWatch::start(cgroup.dir.join("memory.events")) {
            Ok(watch) => Some(watch),
            Err(error) => {
                eprintln!("warning: {error:#}; a kill just before the job ends can go unseen");
                None
            }
        };
        Self {
            group,
            cgroup,
            holder,
            watch,
            _lock: lock,
        }
    }

    pub(crate) fn cgroup(&self) -> &ScopeCgroup {
        &self.cgroup
    }

    /// Reads the scope's use, pressure and out-of-memory counters.
    pub(crate) fn read(&self) -> Result<JobReadings> {
        let dir = &self.cgroup.dir;
        let events = parse_memory_events(&read_file(&dir.join("memory.events"))?)?;
        Ok(JobReadings {
            current_bytes: parse_bytes(&read_file(&dir.join("memory.current"))?)?,
            // Without swap accounting the file does not exist.
            swap_current_bytes: match fs::read_to_string(dir.join("memory.swap.current")) {
                Ok(text) => parse_bytes(&text)?,
                Err(_) => 0,
            },
            full_avg10: read_file(&dir.join("memory.pressure"))
                .ok()
                .and_then(|text| parse_psi_full_avg10(&text).ok()),
            events: self.merge(events),
        })
    }

    /// The counters after the main process ended. systemd may have removed
    /// the scope by then; the watch has the counters it saw.
    pub(crate) fn final_events(&self) -> MemoryEvents {
        let events = read_file(&self.cgroup.dir.join("memory.events"))
            .ok()
            .and_then(|text| parse_memory_events(&text).ok())
            .unwrap_or_default();
        self.merge(events)
    }

    fn merge(&self, events: MemoryEvents) -> MemoryEvents {
        match &self.watch {
            Some(watch) => merge(&watch.seen, events),
            None => events,
        }
    }

    /// Tells whether any process of the job, in the scope or in the group,
    /// still runs; the holder of the scope does not count.
    pub(crate) fn has_members(&self) -> bool {
        scope_members(&self.cgroup)
            .iter()
            .any(|pid| u32::try_from(pid.as_raw()).ok() != self.holder)
            || process_group_exists(self.group.pgid)
    }

    /// `SIGTERM` to every process of the scope, `term_grace`, then
    /// `cgroup.kill`.
    pub(crate) fn terminate(&mut self, term_grace: Duration) {
        terminate_scope(self, term_grace);
    }

    /// `cgroup.kill` at once, as after a kill at the job's limit.
    pub(crate) fn kill(&mut self) {
        kill_scope(self);
    }
}

/// The processes that the kernel places in the scope. A process ID from a
/// file that the kernel did not write, as in a test's fake tree, is never
/// a target: the real `/proc/<pid>/cgroup` must name the scope too.
// Chooses signal targets; `.cargo/mutants.toml` keeps cargo-mutants out of it.
#[cfg_attr(test, mutants::skip)]
fn scope_members(cgroup: &ScopeCgroup) -> Vec<ProcessId> {
    let Ok(text) = fs::read_to_string(cgroup.dir.join("cgroup.procs")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| ProcessId::parse(line.trim()))
        .filter(|pid| scope_member_runs_in(*pid, &cgroup.path))
        .collect()
}

// Chooses signal targets; `.cargo/mutants.toml` keeps cargo-mutants out of it.
#[cfg_attr(test, mutants::skip)]
fn scope_member_runs_in(pid: ProcessId, path: &str) -> bool {
    fs::read_to_string(format!("/proc/{}/cgroup", pid.as_raw()))
        .ok()
        .and_then(|text| parse_cgroup_v2_path(&text))
        .as_deref()
        == Some(path)
}

// Chooses signal targets; `.cargo/mutants.toml` keeps cargo-mutants out of it.
#[cfg_attr(test, mutants::skip)]
fn signal_scope(scope: &Scope, signal: Signal) {
    let _ = signal_process_group(scope.group.pgid, signal);
    for pid in scope_members(&scope.cgroup) {
        let _ = signal_process(pid, signal);
    }
}

// Sends signals; the integration suite covers it with the fake scope, and
// the real-system suite on CI with real ones.
#[cfg_attr(test, mutants::skip)]
fn terminate_scope(scope: &mut Scope, term_grace: Duration) {
    signal_scope(scope, Signal::SIGTERM);
    if scope.group.stopped {
        // A stopped process handles SIGTERM only once it runs again.
        signal_scope(scope, Signal::SIGCONT);
        scope.group.stopped = false;
    }
    let started = Instant::now();
    while started.elapsed() < term_grace {
        let _ = scope.group.poll();
        if !scope.has_members() {
            return;
        }
        thread::sleep(SETTLE_STEP);
    }
    kill_scope(scope);
}

// Kills; see `terminate_scope`.
#[cfg_attr(test, mutants::skip)]
fn kill_scope(scope: &mut Scope) {
    write_cgroup_kill(&scope.cgroup.dir);
    signal_scope(scope, Signal::SIGKILL);
    let started = Instant::now();
    while started.elapsed() < SCOPE_KILL_WAIT {
        let _ = scope.group.poll();
        if !scope.has_members() {
            return;
        }
        thread::sleep(SETTLE_STEP);
    }
    let _ = scope.group.poll();
}

/// Writes `1` to `cgroup.kill`, which the kernel turns into `SIGKILL` for
/// every process of the cgroup. Only in a folder of an `oomwrap-*.scope`.
// Kills; `.cargo/mutants.toml` keeps cargo-mutants out of it.
#[cfg_attr(test, mutants::skip)]
fn write_cgroup_kill(dir: &Path) {
    if !is_own_scope_dir(dir) {
        eprintln!(
            "warning: refusing to write cgroup.kill outside an oomwrap scope: {}",
            dir.display()
        );
        return;
    }
    // Never create the file: a folder without it is no cgroup.
    let written = OpenOptions::new()
        .write(true)
        .open(dir.join("cgroup.kill"))
        .and_then(|mut file| file.write_all(b"1"));
    if let Err(error) = written
        && dir.exists()
    {
        eprintln!("warning: failed to write cgroup.kill: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// A fake scope folder with the files that the guard reads.
    fn fake_scope(root: &Path, name: &str) -> ScopeCgroup {
        let path = format!("/app.slice/{name}");
        let dir = root.join(path.trim_start_matches('/'));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("memory.events"), "oom 0\noom_kill 0\n").unwrap();
        fs::write(dir.join("memory.current"), "4096\n").unwrap();
        ScopeCgroup { path, dir }
    }

    fn sleeper() -> ProcessGroup {
        ProcessGroup::spawn(OsStr::new("sleep"), &["30".into()]).unwrap()
    }

    #[test]
    fn a_lock_is_held_once_and_goes_with_its_guard() {
        let dir = tempfile::tempdir().unwrap();
        let locks = dir.path().join("oomwrap");
        let lock = ScopeLock::acquire(&locks, "oomwrap-g-1").unwrap();
        let path = locks.join("oomwrap-g-1.lock");
        assert!(path.exists());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&locks).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(ScopeLock::acquire(&locks, "oomwrap-g-1").is_err());
        let other = File::open(&path).unwrap();
        assert!(other.try_lock().is_err());
        drop(lock);
        assert!(!path.exists());
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(ScopeLock::acquire(&file, "u").is_err());
    }

    #[test]
    fn the_watch_keeps_counters_that_rise_before_the_scope_goes() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = fake_scope(dir.path(), "oomwrap-g-1.scope");
        let lock = ScopeLock::acquire(&dir.path().join("locks"), "oomwrap-g-1").unwrap();
        let mut scope = Scope::new(sleeper(), cgroup.clone(), None, lock);
        let first = scope.read().unwrap();
        assert_eq!(first.current_bytes, 4096);
        assert_eq!(first.swap_current_bytes, 0);
        assert_eq!(first.full_avg10, None);
        assert_eq!(first.events, MemoryEvents::default());
        fs::write(cgroup.dir.join("memory.events"), "oom 1\noom_kill 1\n").unwrap();
        let seen = wait_until(|| {
            scope.watch.as_ref().is_some_and(|watch| {
                *watch.seen.lock().unwrap()
                    == MemoryEvents {
                        oom: 1,
                        oom_kill: 1,
                    }
            })
        });
        // systemd removes the scope's cgroup once it is empty.
        fs::rename(&cgroup.dir, dir.path().join("gone")).unwrap();
        let after = scope.final_events();
        assert!(scope.read().is_err());
        scope.group.kill();
        assert!(seen, "the watch must see the new counters");
        assert_eq!(
            after,
            MemoryEvents {
                oom: 1,
                oom_kill: 1
            }
        );
    }

    #[test]
    fn a_reading_never_lowers_a_counter() {
        let seen = Mutex::new(MemoryEvents {
            oom: 2,
            oom_kill: 0,
        });
        assert_eq!(
            merge(
                &seen,
                MemoryEvents {
                    oom: 1,
                    oom_kill: 3
                }
            ),
            MemoryEvents {
                oom: 2,
                oom_kill: 3
            }
        );
    }

    #[test]
    fn a_scope_without_its_files_reads_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = fake_scope(dir.path(), "oomwrap-g-2.scope");
        fs::write(cgroup.dir.join("memory.swap.current"), "8192\n").unwrap();
        fs::write(
            cgroup.dir.join("memory.pressure"),
            "some avg10=1.00 avg60=0 avg300=0 total=0\nfull avg10=70.00 avg60=0 avg300=0 total=0\n",
        )
        .unwrap();
        let lock = ScopeLock::acquire(&dir.path().join("locks"), "oomwrap-g-2").unwrap();
        let mut scope = Scope::new(sleeper(), cgroup.clone(), None, lock);
        let reading = scope.read().unwrap();
        assert_eq!(reading.swap_current_bytes, 8192);
        assert_eq!(reading.full_avg10.unwrap().hundredths(), 7000);
        assert_eq!(scope.cgroup(), &cgroup);
        fs::write(cgroup.dir.join("memory.swap.current"), "x\n").unwrap();
        assert!(scope.read().is_err());
        fs::remove_file(cgroup.dir.join("memory.current")).unwrap();
        assert!(scope.read().is_err());
        fs::write(cgroup.dir.join("memory.events"), "oom x\n").unwrap();
        assert!(scope.read().is_err());
        assert_eq!(scope.final_events(), MemoryEvents::default());
        scope.group.kill();
    }

    #[test]
    fn only_processes_that_the_kernel_places_in_the_scope_are_members() {
        let dir = tempfile::tempdir().unwrap();
        let mut group = sleeper();
        let pid = group.leader_pid();
        let real =
            parse_cgroup_v2_path(&fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap())
                .unwrap();
        // A fake file that names the own child, at a path where it does not run.
        let fake = fake_scope(dir.path(), "oomwrap-g-3.scope");
        fs::write(
            fake.dir.join("cgroup.procs"),
            format!("{pid}\n1\n0\n-1\nx\n"),
        )
        .unwrap();
        let fake_members = scope_members(&fake);
        // The same file at the child's real cgroup path.
        let real_dir = dir.path().join("real").join(real.trim_start_matches('/'));
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("cgroup.procs"), format!("{pid}\n1\n")).unwrap();
        let real_members = scope_members(&ScopeCgroup {
            path: real,
            dir: real_dir,
        });
        group.kill();
        assert!(fake_members.is_empty());
        assert_eq!(real_members.len(), 1);
        assert_eq!(i64::from(real_members[0].as_raw()), i64::from(pid));
        assert!(
            scope_members(&ScopeCgroup {
                path: "/none".to_string(),
                dir: dir.path().join("none"),
            })
            .is_empty()
        );
    }
}

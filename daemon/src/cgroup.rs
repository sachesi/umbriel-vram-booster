//! Reading dmem capacity and writing `dmem.low` and `dmem.max`, plus the
//! `/proc` lookups that map a pid to its systemd unit cgroup.

use std::fs;
use std::time::{Duration, Instant};
use tracing::warn;

pub(crate) fn parse_dmem_capacity(content: &str) -> Vec<(String, u64)> {
    content
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let key = parts.next()?;
            let val: u64 = parts.next()?.parse().ok()?;
            if key.starts_with("drm/") && val > 0 {
                Some((key.to_string(), val))
            } else {
                None
            }
        })
        .collect()
}

/// The GPU to boost: the largest drm entry in `dmem.capacity`, or the one
/// `DRM_KEY` names. The error is the message to show the user, since every
/// failure here has a different cause and a different fix.
pub(crate) fn read_dmem_capacity() -> Result<(String, u64), String> {
    let content = fs::read_to_string("/sys/fs/cgroup/dmem.capacity").map_err(|e| {
        format!(
            "cannot read /sys/fs/cgroup/dmem.capacity: {e}. Does this kernel have the dmem controller (6.12+)?"
        )
    })?;
    let entries = parse_dmem_capacity(&content);
    if entries.is_empty() {
        return Err(
            "no drm entries in /sys/fs/cgroup/dmem.capacity. Is dmemcg-booster.service running?"
                .to_string(),
        );
    }
    match std::env::var("DRM_KEY") {
        Ok(wanted) => entries
            .iter()
            .find(|(k, _)| *k == wanted)
            .cloned()
            .ok_or_else(|| {
                let known: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
                format!(
                    "DRM_KEY={wanted} is not in dmem.capacity, which lists: {}",
                    known.join(", ")
                )
            }),
        Err(_) => entries
            .into_iter()
            .max_by_key(|(_, v)| *v)
            .ok_or_else(|| "no usable drm entry in dmem.capacity".to_string()),
    }
}

pub(crate) fn cgroup_path_for_pid(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    for line in text.lines() {
        if let Some(rel) = line.strip_prefix("0::") {
            return Some(format!("/sys/fs/cgroup{}", rel.trim()));
        }
    }
    None
}

/// What a dmem write did, so a caller can tell a cgroup that has no such
/// file from one whose write did not finish in time.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Wrote,
    Missing,
    TimedOut,
}

/// One dmem write, carried out on the writer thread.
struct Job {
    file: String,
    body: String,
    /// Open with O_NONBLOCK. For `dmem.max` that stops a kernel which
    /// reclaims down to a lowered limit (7.3 and later) from evicting in the
    /// write: the limit takes effect at once, and usage shrinks as buffers
    /// are freed. Older kernels ignore the flag.
    nonblock: bool,
    done: tokio::sync::oneshot::Sender<std::io::Result<WriteOutcome>>,
}

fn write_file(file: &str, body: &str, nonblock: bool) -> std::io::Result<()> {
    if !nonblock {
        return fs::write(file, body);
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(file)?
        .write_all(body.as_bytes())
}

/// Writes dmem files on a thread of its own, one at a time and in the
/// order they were asked for. A write that hangs holds up the ones behind it
/// instead of being overtaken, so a clear queued after a boost can never
/// land first and leave the boost in place. Waiting for a write is bounded;
/// the write itself is not, since a blocking write cannot be cancelled.
pub(crate) struct DmemWriter {
    jobs: std::sync::mpsc::Sender<Job>,
    timeout: Duration,
}

impl DmemWriter {
    pub(crate) fn new(timeout: Duration) -> Self {
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            for job in queue {
                let outcome = if fs::metadata(&job.file).is_err() {
                    Ok(WriteOutcome::Missing)
                } else {
                    write_file(&job.file, &job.body, job.nonblock).map(|()| WriteOutcome::Wrote)
                };
                let _ = job.done.send(outcome);
            }
        });
        Self { jobs, timeout }
    }

    /// Set `dmem.low` of `cgroup_dir` for `drm_key`.
    pub(crate) async fn write(
        &self,
        cgroup_dir: &str,
        drm_key: &str,
        bytes: u64,
    ) -> std::io::Result<WriteOutcome> {
        self.queue(
            cgroup_dir,
            "dmem.low",
            format!("{drm_key} {bytes}\n"),
            false,
        )
        .await
    }

    /// Set `dmem.max` of `cgroup_dir` for `drm_key`; None lifts it.
    pub(crate) async fn write_max(
        &self,
        cgroup_dir: &str,
        drm_key: &str,
        bytes: Option<u64>,
    ) -> std::io::Result<WriteOutcome> {
        let value = bytes.map_or_else(|| "max".to_string(), |b| b.to_string());
        self.queue(cgroup_dir, "dmem.max", format!("{drm_key} {value}\n"), true)
            .await
    }

    async fn queue(
        &self,
        cgroup_dir: &str,
        name: &str,
        body: String,
        nonblock: bool,
    ) -> std::io::Result<WriteOutcome> {
        if cgroup_dir.contains("..") {
            return Ok(WriteOutcome::Missing);
        }
        let gone = || std::io::Error::other("the dmem writer thread is gone");
        let (done, outcome) = tokio::sync::oneshot::channel();
        let job = Job {
            file: format!("{cgroup_dir}/{name}"),
            body,
            nonblock,
            done,
        };
        self.jobs.send(job).map_err(|_| gone())?;
        match tokio::time::timeout(self.timeout, outcome).await {
            Ok(result) => result.map_err(|_| gone())?,
            Err(_) => Ok(WriteOutcome::TimedOut),
        }
    }
}

/// True if the cgroup's `dmem.low` currently holds `value` for `drm_key`.
/// Used to notice a boost that something else reverted.
pub(crate) async fn dmem_low_is(cgroup_dir: &str, drm_key: &str, value: u64) -> bool {
    match tokio::fs::read_to_string(format!("{cgroup_dir}/dmem.low")).await {
        Ok(content) => dmem_low_has_value(&content, drm_key, value),
        Err(_) => false,
    }
}

pub(crate) fn is_app_scope(cgroup_dir: &str) -> bool {
    cgroup_dir.split('/').any(|c| c == "app.slice")
}

/// The unit a cgroup under `app.slice` belongs to: the first component below
/// it, through any sub-slices, that is not itself a slice. A process can sit
/// deeper, in a cgroup a delegated unit made for itself; dmem protection is
/// recursive, so boosting the unit covers it, and the unit is also what
/// app_id matching resolves to. None outside `app.slice`, or in a slice.
pub(crate) fn app_unit_cgroup(cgroup_dir: &str) -> Option<String> {
    let (head, rest) = cgroup_dir.split_once("/app.slice/")?;
    let mut unit = format!("{head}/app.slice");
    for part in rest.split('/') {
        unit.push('/');
        unit.push_str(part);
        if !part.ends_with(".slice") {
            return Some(unit);
        }
    }
    None
}

/// What `content` (a dmem.low or dmem.max file body) sets `drm_key` to: a
/// number of bytes, or `max`. None if the region is not listed.
pub(crate) fn dmem_entry<'a>(content: &'a str, drm_key: &str) -> Option<&'a str> {
    content.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(k), Some(v)) if k == drm_key => Some(v),
            _ => None,
        }
    })
}

/// True if `content` (a dmem.low file body) sets `drm_key` to exactly `value`.
pub(crate) fn dmem_low_has_value(content: &str, drm_key: &str, value: u64) -> bool {
    content.lines().any(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(k), Some(v)) => k == drm_key && v.parse::<u64>() == Ok(value),
            _ => false,
        }
    })
}

/// uid of the session this daemon runs as (owner of /proc/self). Used to scope
/// startup cleanup to our own user slice so we never touch other users' scopes.
/// There is no sensible default: uid 0 would point the daemon at root's slice.
pub(crate) fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").map(|m| m.uid()).ok()
}

/// Best-effort startup cleanup: clear dmem.low values left behind by a crashed
/// or SIGKILLed daemon. Only clears app.slice scopes whose value for the selected
/// drm_key equals our boost value; unrelated values are left untouched. Scoped to
/// `root` (our own user-<uid>.slice) to avoid touching other users' cgroups.
pub(crate) fn cleanup_stale_boosts(
    root: &std::path::Path,
    drm_key: &str,
    boost_bytes: u64,
) -> usize {
    fn walk(dir: &std::path::Path, drm_key: &str, boost_bytes: u64, cleared: &mut usize) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                walk(&path, drm_key, boost_bytes, cleared);
            } else if entry.file_name() == "dmem.low" && is_app_scope(&path.to_string_lossy()) {
                let content = match fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if dmem_low_has_value(&content, drm_key, boost_bytes) {
                    match fs::write(&path, format!("{drm_key} 0\n")) {
                        Ok(()) => *cleared += 1,
                        Err(e) => warn!("startup cleanup: failed to clear {}: {e}", path.display()),
                    }
                }
            }
        }
    }
    let mut cleared = 0;
    walk(root, drm_key, boost_bytes, &mut cleared);
    cleared
}

pub(crate) fn unit_label(cgroup_dir: &str) -> &str {
    cgroup_dir.rsplit('/').next().unwrap_or(cgroup_dir)
}

pub(crate) fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub(crate) fn pid_comm(pid: u32) -> String {
    read_trimmed(&format!("/proc/{pid}/comm")).unwrap_or_default()
}

/// The app.slice unit cgroup of `pid`, or of one of its descendants up to
/// `max_depth`. Launchers commonly sit outside `app.slice` and put the app
/// they started into a scope of its own.
///
/// The search is bounded three ways: by depth, by `MAX_CHILDREN` per level,
/// and by `deadline` - a supervisor with hundreds of children would otherwise
/// turn one focus event into thousands of `/proc` reads.
pub(crate) fn find_app_scope_for_pid(
    pid: u32,
    max_depth: usize,
    deadline: Instant,
) -> Option<String> {
    const MAX_CHILDREN: usize = 64;

    fn check(pid: u32, depth: usize, max_depth: usize, deadline: Instant) -> Option<String> {
        if Instant::now() >= deadline {
            return None;
        }
        if let Some(cg) = cgroup_path_for_pid(pid).and_then(|c| app_unit_cgroup(&c)) {
            return Some(cg);
        }
        if depth >= max_depth {
            return None;
        }
        let task_dir = format!("/proc/{pid}/task");
        let task_entries = fs::read_dir(&task_dir).ok()?;
        let mut visited = 0;
        for entry in task_entries.flatten() {
            let tid: u32 = match entry.file_name().to_string_lossy().parse() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let children_str = match read_trimmed(&format!("/proc/{pid}/task/{tid}/children")) {
                Some(s) => s,
                None => continue,
            };
            for child_str in children_str.split_whitespace() {
                let child: u32 = match child_str.parse() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Some(cg) = check(child, depth + 1, max_depth, deadline) {
                    return Some(cg);
                }
                visited += 1;
                if visited >= MAX_CHILDREN {
                    return None;
                }
            }
        }
        None
    }
    check(pid, 0, max_depth, deadline)
}

/// A FIFO as `dmem.low`: every write to it blocks until it is read, which
/// makes a hung write on demand.
#[cfg(test)]
pub(crate) mod fifo {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub(crate) fn make(dir: &Path) -> PathBuf {
        let fifo = dir.join("dmem.low");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.unwrap().success());
        fifo
    }

    /// Read `lines` lines from the FIFO on a thread of its own, through one
    /// descriptor. Reopening it per write would race the next write: one that
    /// opens just before the reader closes lands in a pipe nobody reads, and
    /// the reader waits for it forever.
    pub(crate) fn read_lines(
        fifo: PathBuf,
        lines: usize,
    ) -> tokio::sync::oneshot::Receiver<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut file = std::fs::File::open(&fifo).unwrap();
            let mut seen = String::new();
            let mut buf = [0u8; 256];
            while seen.lines().count() < lines {
                match file.read(&mut buf).unwrap() {
                    // no writer at the moment; the next one does not wait,
                    // since this descriptor keeps a reader on the FIFO
                    0 => std::thread::sleep(Duration::from_millis(1)),
                    n => seen.push_str(std::str::from_utf8(&buf[..n]).unwrap()),
                }
            }
            let _ = tx.send(seen);
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dmem_capacity_picks_drm_entries() {
        let content = "drm/0000:2d:00.0/vram 8573157376\ndrm/0000:2d:00.0/gtt 0\nsystem 12345\n";
        let entries = parse_dmem_capacity(content);
        assert_eq!(
            entries,
            vec![("drm/0000:2d:00.0/vram".to_string(), 8573157376)]
        );
    }

    #[test]
    fn parse_dmem_capacity_ignores_malformed() {
        assert!(parse_dmem_capacity("").is_empty());
        assert!(parse_dmem_capacity("drm/x/vram notanumber\njunk\n").is_empty());
    }

    #[test]
    fn is_app_scope_exact_component_only() {
        assert!(is_app_scope(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo.scope"
        ));
        assert!(!is_app_scope("/sys/fs/cgroup/user.slice/session.slice"));
        // substring that is not an exact path component must not match
        assert!(!is_app_scope("/sys/fs/cgroup/my-app.slice-x/foo"));
    }

    #[test]
    fn app_unit_cgroup_stops_at_the_unit() {
        let app = "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice";
        let unit = format!("{app}/app-foo.scope");
        assert_eq!(app_unit_cgroup(&unit), Some(unit.clone()));
        // a cgroup a delegated unit made for itself resolves to the unit
        assert_eq!(
            app_unit_cgroup(&format!("{unit}/container/payload")),
            Some(unit)
        );
        let dbus = format!(
            "{app}/app-dbus\\x2d:1.2\\x2dorg.gnome.Loupe.slice/dbus-:1.2-org.gnome.Loupe@0.service"
        );
        assert_eq!(app_unit_cgroup(&dbus), Some(dbus.clone()));
        // a slice is never the unit, nor is anything outside app.slice
        assert_eq!(app_unit_cgroup(&format!("{app}/app-x.slice")), None);
        assert_eq!(app_unit_cgroup(app), None);
        assert_eq!(
            app_unit_cgroup("/sys/fs/cgroup/user.slice/user-1000.slice/session-2.scope"),
            None
        );
    }

    #[test]
    fn dmem_low_has_value_matches_exact_key_and_value() {
        let body = "drm/0000:2d:00.0/vram 7715841638\n";
        assert!(dmem_low_has_value(
            body,
            "drm/0000:2d:00.0/vram",
            7715841638
        ));
        assert!(!dmem_low_has_value(body, "drm/0000:2d:00.0/vram", 0));
        assert!(!dmem_low_has_value(body, "drm/other/vram", 7715841638));
        assert!(!dmem_low_has_value("", "drm/0000:2d:00.0/vram", 7715841638));
    }

    /// A FIFO as `dmem.low` blocks a write until someone reads it, which is a
    /// hung write on demand: the one behind it must wait, not overtake it.
    #[tokio::test]
    async fn a_hung_write_is_not_overtaken_by_the_next() {
        let dir = std::env::temp_dir().join(format!("uvb-fifo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fifo = fifo::make(&dir);
        let path = dir.to_string_lossy().into_owned();
        let key = "drm/0000:2d:00.0/vram";
        let writer = DmemWriter::new(Duration::from_millis(100));

        // nobody reads the FIFO: the boost hangs, and waiting for it gives up
        assert_eq!(
            writer.write(&path, key, 7715841638).await.unwrap(),
            WriteOutcome::TimedOut
        );
        // the clear is queued behind it before anything reads the FIFO
        let clear = writer.write(&path, key, 0);
        // started only once the clear is queued; a plain thread, so a read
        // that never ends fails on the timeout instead of hanging the test
        let read = async move {
            tokio::time::timeout(Duration::from_secs(5), fifo::read_lines(fifo, 2)).await
        };
        let (_, seen) = tokio::join!(clear, read);
        let seen = seen.expect("the FIFO never saw both writes").unwrap();
        assert_eq!(seen, format!("{key} 7715841638\n{key} 0\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `dmem.low` is an ordinary file as far as this code is concerned, so the
    /// boost / revert / re-apply cycle can be exercised in a temp directory.
    #[tokio::test]
    async fn a_boost_survives_a_revert_from_outside() {
        let dir = std::env::temp_dir().join(format!("uvb-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().into_owned();
        let key = "drm/0000:2d:00.0/vram";
        let writer = DmemWriter::new(Duration::from_secs(2));

        // no dmem.low yet: a write must say so rather than claim success
        assert_eq!(
            writer.write(&path, key, 7715841638).await.unwrap(),
            WriteOutcome::Missing
        );
        assert!(!dmem_low_is(&path, key, 7715841638).await);

        fs::write(dir.join("dmem.low"), format!("{key} 0\n")).unwrap();
        assert_eq!(
            writer.write(&path, key, 7715841638).await.unwrap(),
            WriteOutcome::Wrote
        );
        assert!(dmem_low_is(&path, key, 7715841638).await);

        // something else reverts it: the daemon must be able to notice
        fs::write(dir.join("dmem.low"), format!("{key} 0\n")).unwrap();
        assert!(!dmem_low_is(&path, key, 7715841638).await);
        assert!(!dmem_low_is(&path, "drm/other/vram", 7715841638).await);

        assert_eq!(
            writer.write(&path, key, 0).await.unwrap(),
            WriteOutcome::Wrote
        );
        assert_eq!(
            fs::read_to_string(dir.join("dmem.low")).unwrap(),
            format!("{key} 0\n")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

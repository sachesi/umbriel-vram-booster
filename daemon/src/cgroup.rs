//! Reading dmem capacity and writing `dmem.low`, plus the `/proc` lookups
//! that map a pid to its systemd unit cgroup.

use std::fs;
use std::time::Instant;
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

/// What a `dmem.low` write did, so a caller can tell a scope that has no
/// `dmem.low` from one whose write did not finish in time.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Wrote,
    Missing,
    TimedOut,
}

pub(crate) async fn write_dmem_low(
    cgroup_dir: &str,
    drm_key: &str,
    bytes: u64,
) -> std::io::Result<WriteOutcome> {
    if cgroup_dir.contains("..") {
        return Ok(WriteOutcome::Missing);
    }
    let file = format!("{cgroup_dir}/dmem.low");
    let drm_key = drm_key.to_string();
    match tokio::time::timeout(std::time::Duration::from_secs(2), async move {
        if tokio::fs::metadata(&file).await.is_err() {
            return Ok::<WriteOutcome, std::io::Error>(WriteOutcome::Missing);
        }
        tokio::fs::write(&file, format!("{drm_key} {bytes}\n")).await?;
        Ok(WriteOutcome::Wrote)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(WriteOutcome::TimedOut),
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

/// The app.slice cgroup of `pid`, or of one of its descendants up to
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
        if let Some(cg) = cgroup_path_for_pid(pid)
            && is_app_scope(&cg)
        {
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
}

//! Umbriel's IPC socket: where it lives and what to take from a `windows`
//! snapshot.

use std::fs;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::watch;
use tracing::warn;

/// Umbriel's IPC socket: `UMBRIEL_SOCKET`, else derived from `WAYLAND_DISPLAY`,
/// else the newest `umbriel-*.sock` in the runtime dir, since a systemd user
/// service does not always inherit the compositor's environment.
pub(crate) fn umbriel_socket_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("UMBRIEL_SOCKET") {
        return Some(p.into());
    }
    let run = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let display = std::env::var("WAYLAND_DISPLAY").ok();
    socket_in_runtime_dir(std::path::Path::new(&run), display.as_deref())
}

/// The derived socket counts only if it exists: the user manager can hold a
/// `WAYLAND_DISPLAY` from another compositor or an earlier session, and an
/// absolute one names no `umbriel-*.sock` at all. Either way the scan decides.
fn socket_in_runtime_dir(
    run: &std::path::Path,
    display: Option<&str>,
) -> Option<std::path::PathBuf> {
    if let Some(display) = display.filter(|d| !d.is_empty() && !d.contains('/')) {
        let derived = run.join(format!("umbriel-{display}.sock"));
        if derived.exists() {
            return Some(derived);
        }
    }
    fs::read_dir(run)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("umbriel-") && n.ends_with(".sock")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Read Umbriel's stream until it ends, putting the data of each `windows`
/// snapshot on `tx`. Reading never waits for a snapshot to be acted on: only
/// the newest one matters, so one that is not taken in time is overwritten.
/// Some(message) if Umbriel rejected the subscription.
pub(crate) async fn read_snapshots<R: AsyncRead + Unpin>(
    stream: R,
    tx: watch::Sender<serde_json::Value>,
) -> Option<String> {
    /// A `windows` snapshot is a few kilobytes. Reading further than this
    /// would grow the buffer without a bound the peer respects.
    const MAX_LINE: u64 = 1 << 20;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let read = {
            let mut limited = (&mut reader).take(MAX_LINE);
            limited.read_line(&mut line).await
        };
        match read {
            Ok(_) if line.is_empty() => return None,
            Ok(_) if !line.ends_with('\n') => {
                warn!("Umbriel sent more than {MAX_LINE} bytes without a newline; reconnecting");
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("reading from Umbriel failed: {e}");
                return None;
            }
        }
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(err) = v["err"].as_str() {
            return Some(err.to_string());
        }
        if v["event"] == "windows" {
            tx.send_replace(v["data"].take());
        }
    }
}

/// The window to boost from one `windows` snapshot: the seat-global `active`
/// entry (`focused` is per workspace). Its pid counts only for a native
/// Wayland client; X11 windows report the xwayland-satellite bridge, or -1.
pub(crate) fn pick_window(windows: &serde_json::Value) -> Option<(Option<u32>, String)> {
    let active = windows.as_array()?.iter().find(|w| w["active"] == true)?;
    let pid = active["pid"]
        .as_i64()
        .filter(|_| active["xwayland"] != true)
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p > 0);
    let app_id = active["app_id"].as_str().unwrap_or("").to_string();
    if pid.is_none() && app_id.is_empty() {
        return None;
    }
    Some((pid, app_id))
}

/// How long a window stays deduplicated after a successful boost. Repeated
/// snapshots for the same window cost nothing, but the boosted cgroup's
/// `dmem.low` is read again this often so a boost reverted from outside (a
/// `dmemcg-booster` restart, a stray write) is noticed while focus stays put.
const RECHECK: Duration = Duration::from_secs(5);

/// How long a window that resolved to no unit waits before another attempt.
/// A scope can appear moments after its window does, so failures are retried -
/// but a snapshot arrives on every title change, and rescanning `app.slice`
/// on each one would keep the daemon busy for as long as the window is focused.
const RETRY: Duration = Duration::from_secs(2);

/// What to do about one `windows` snapshot.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Boost {
        pid: Option<u32>,
        app_id: String,
        key: String,
    },
    /// Same window, and its boost is due for a check. Resolving the window
    /// again would mean another walk through `app.slice`; only the cgroup
    /// already boosted needs looking at, which is one file read.
    Verify { key: String },
    /// No window is active: drop any boost.
    Clear,
    /// Same window as last time, and it is not due for another look.
    Skip,
}

/// Decides what each snapshot is worth acting on. Keeping this out of the
/// socket loop is what makes the sequence testable.
#[derive(Default)]
pub(crate) struct Tracker {
    boosted: Option<(String, Instant)>,
    failed: Option<(String, Instant)>,
}

impl Tracker {
    pub(crate) fn next(&mut self, windows: &serde_json::Value, now: Instant) -> Action {
        let Some((pid, app_id)) = pick_window(windows) else {
            self.boosted = None;
            self.failed = None;
            return Action::Clear;
        };
        let key = match pid {
            Some(p) => format!("pid:{p}"),
            None => format!("app:{app_id}"),
        };
        let boosted_due = match &self.boosted {
            Some((k, until)) if *k == key => Some(now >= *until),
            _ => None,
        };
        match boosted_due {
            Some(false) => return Action::Skip,
            Some(true) => {
                self.boosted = Some((key.clone(), now + RECHECK));
                return Action::Verify { key };
            }
            None => {}
        }
        if matches!(&self.failed, Some((k, until)) if *k == key && now < *until) {
            return Action::Skip;
        }
        Action::Boost { pid, app_id, key }
    }

    /// Remember the outcome of the boost the last `next` asked for.
    pub(crate) fn record(&mut self, key: String, boosted: bool, now: Instant) {
        if boosted {
            self.boosted = Some((key, now + RECHECK));
            self.failed = None;
        } else {
            self.failed = Some((key, now + RETRY));
            self.boosted = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_window_uses_active_and_native_pid_only() {
        let snap = serde_json::json!([
            {"app_id": "kitty", "focused": true, "active": false, "pid": 11},
            {"app_id": "org.mozilla.firefox", "focused": true, "active": true, "pid": 22, "xwayland": false},
            {"app_id": "steam", "focused": true, "active": false, "pid": -1, "xwayland": true},
        ]);
        assert_eq!(
            pick_window(&snap),
            Some((Some(22), "org.mozilla.firefox".to_string()))
        );
        let x11 = serde_json::json!([{"app_id": "steam_app_1", "active": true, "pid": 4242, "xwayland": true}]);
        assert_eq!(pick_window(&x11), Some((None, "steam_app_1".to_string())));
        let bare = serde_json::json!([{"active": true, "pid": -1}]);
        assert_eq!(pick_window(&bare), None);
        assert_eq!(pick_window(&serde_json::json!([])), None);
        assert_eq!(pick_window(&serde_json::Value::Null), None);
        let pid_only = serde_json::json!([{"active": true, "pid": 7}]);
        assert_eq!(pick_window(&pid_only), Some((Some(7), String::new())));
    }

    #[test]
    fn socket_falls_back_to_the_scan_when_the_derived_one_is_missing() {
        let run = std::env::temp_dir().join(format!("uvb-sock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run);
        fs::create_dir_all(&run).unwrap();
        assert_eq!(socket_in_runtime_dir(&run, Some("wayland-1")), None);

        let live = run.join("umbriel-wayland-2.sock");
        fs::write(&live, "").unwrap();
        // a stale or foreign WAYLAND_DISPLAY must not hide the live socket
        assert_eq!(
            socket_in_runtime_dir(&run, Some("wayland-0")),
            Some(live.clone())
        );
        assert_eq!(
            socket_in_runtime_dir(&run, Some("/tmp/wl.sock")),
            Some(live.clone())
        );
        assert_eq!(socket_in_runtime_dir(&run, None), Some(live.clone()));

        let derived = run.join("umbriel-wayland-1.sock");
        fs::write(&derived, "").unwrap();
        assert_eq!(
            socket_in_runtime_dir(&run, Some("wayland-1")),
            Some(derived)
        );
        let _ = fs::remove_dir_all(&run);
    }

    #[tokio::test]
    async fn snapshots_are_read_while_nothing_acts_on_them() {
        use tokio::io::AsyncWriteExt;
        // a pipe far smaller than what is sent: the writes only finish if the
        // reader drains it without anyone taking the snapshots
        let (mut umbriel, daemon) = tokio::io::duplex(64);
        let (tx, rx) = watch::channel(serde_json::Value::Null);
        let reader = tokio::spawn(read_snapshots(daemon, tx));
        let send = async {
            umbriel
                .write_all(b"{\"ok\":true}\nnot json\n")
                .await
                .unwrap();
            for n in 0..100 {
                let line = format!("{{\"event\":\"windows\",\"data\":[{{\"n\":{n}}}]}}\n");
                umbriel.write_all(line.as_bytes()).await.unwrap();
            }
            umbriel
                .write_all(b"{\"event\":\"workspaces\",\"data\":[]}\n")
                .await
                .unwrap();
            drop(umbriel);
        };
        tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the reader stopped draining the stream");
        assert_eq!(reader.await.unwrap(), None);
        assert_eq!(*rx.borrow(), serde_json::json!([{"n": 99}]));
    }

    #[tokio::test]
    async fn a_rejected_subscription_ends_the_read_with_the_reason() {
        let stream: &[u8] = b"{\"err\":\"unknown subscription event: windows\"}\n";
        let (tx, _rx) = watch::channel(serde_json::Value::Null);
        assert_eq!(
            read_snapshots(stream, tx).await.as_deref(),
            Some("unknown subscription event: windows")
        );
    }

    fn snapshot(app_id: &str, pid: i64) -> serde_json::Value {
        serde_json::json!([{"app_id": app_id, "active": true, "pid": pid}])
    }

    #[test]
    fn tracker_skips_a_boosted_window_until_it_is_due_for_a_recheck() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let snap = snapshot("firefox", 22);

        let Action::Boost { key, pid, .. } = t.next(&snap, t0) else {
            panic!("first snapshot must boost");
        };
        assert_eq!(pid, Some(22));
        t.record(key, true, t0);

        assert_eq!(t.next(&snap, t0 + Duration::from_secs(1)), Action::Skip);

        // due for a check: no re-resolution, and the next snapshot right after
        // is quiet again rather than checking on every event from then on
        let due = t0 + RECHECK + Duration::from_millis(1);
        assert!(matches!(t.next(&snap, due), Action::Verify { .. }));
        assert_eq!(t.next(&snap, due + Duration::from_millis(1)), Action::Skip);
    }

    #[test]
    fn tracker_retries_a_failed_window_on_a_timer_not_every_snapshot() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let snap = snapshot("kitty", -1);

        let Action::Boost { key, .. } = t.next(&snap, t0) else {
            panic!("first snapshot must be tried");
        };
        t.record(key, false, t0);

        // a title change a moment later must not trigger another scan
        assert_eq!(t.next(&snap, t0 + Duration::from_millis(50)), Action::Skip);
        assert!(matches!(
            t.next(&snap, t0 + RETRY + Duration::from_millis(1)),
            Action::Boost { .. }
        ));
    }

    #[test]
    fn tracker_acts_on_a_new_window_at_once_and_clears_on_an_empty_snapshot() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let Action::Boost { key, .. } = t.next(&snapshot("firefox", 22), t0) else {
            panic!("must boost");
        };
        t.record(key, true, t0);

        assert!(matches!(
            t.next(&snapshot("kitty", 33), t0),
            Action::Boost { .. }
        ));
        assert_eq!(t.next(&serde_json::json!([]), t0), Action::Clear);
        // after a clear the previous window is not remembered
        assert!(matches!(
            t.next(&snapshot("firefox", 22), t0),
            Action::Boost { .. }
        ));
    }
}

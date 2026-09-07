use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};
use zbus::{connection, interface};

mod cgroup;
mod matcher;
mod umbriel;

use cgroup::{
    WriteOutcome, cgroup_path_for_pid, cleanup_stale_boosts, current_uid, dmem_low_is,
    find_app_scope_for_pid, pid_comm, read_dmem_capacity, unit_label, write_dmem_low,
};
use matcher::find_app_scope_for_app_id;
use umbriel::{Action, Tracker, umbriel_socket_path};

fn parse_boost_ratio(raw: &str) -> Option<f64> {
    match raw.parse::<f64>() {
        Ok(r) if (0.0..=1.0).contains(&r) => Some(r),
        _ => None,
    }
}

fn read_boost_ratio() -> f64 {
    match std::env::var("VRAM_BOOST_RATIO") {
        Ok(v) => parse_boost_ratio(&v).unwrap_or_else(|| {
            warn!("VRAM_BOOST_RATIO invalid, using 0.90");
            0.90
        }),
        Err(_) => 0.90,
    }
}

/// Trim a string that came from a window or another process before it goes
/// into a log line: control characters would let any app forge journal
/// entries, and an overlong id would bury the rest of the line.
fn loggable(raw: &str) -> String {
    let clean: String = raw.chars().filter(|c| !c.is_control()).collect();
    match clean.char_indices().nth(64) {
        Some((i, _)) => format!("{}\u{2026}", &clean[..i]),
        None => clean,
    }
}

/// The part of the cgroup tree this daemon owns: its own user slice, and the
/// app slice inside it that app_id matching scans.
struct Scope {
    user_root: std::path::PathBuf,
    app_slice: std::path::PathBuf,
}

struct Inner {
    /// Cgroup that currently holds the boost.
    boosted_cgroup: Option<String>,
    /// Unit label of that cgroup, for ctl.
    current_unit: String,
    /// Umbriel socket the follower is reading, empty while it is waiting.
    following: String,
    drm_key: String,
    vram_total: u64,
    boost_ratio: f64,
}

impl Inner {
    fn boost_bytes(&self) -> u64 {
        (self.vram_total as f64 * self.boost_ratio) as u64
    }

    async fn clear_boost(&mut self) {
        if let Some(cgroup) = self.boosted_cgroup.take() {
            let label = unit_label(&cgroup);
            match write_dmem_low(&cgroup, &self.drm_key, 0).await {
                Ok(WriteOutcome::Wrote) => info!("cleared the boost on {label}"),
                Ok(WriteOutcome::Missing) => {
                    info!("nothing to clear on {label}, its scope is gone");
                }
                Ok(WriteOutcome::TimedOut) => {
                    warn!("clearing the boost on {label} did not finish in 2 s");
                }
                Err(e) => warn!("cannot clear the boost on {label}: {e}"),
            }
        }
        self.current_unit.clear();
    }

    /// Re-apply the boost if something else reverted it. One file read while
    /// it is intact, and no window resolution: the cgroup is already known.
    /// False means the caller should resolve the window again.
    async fn verify_boost(&mut self) -> bool {
        let Some(cgroup) = self.boosted_cgroup.clone() else {
            return false;
        };
        let boost = self.boost_bytes();
        if dmem_low_is(&cgroup, &self.drm_key, boost).await {
            return true;
        }
        let label = unit_label(&cgroup).to_string();
        info!("the boost on {label} was reverted from outside, applying it again");
        if matches!(
            write_dmem_low(&cgroup, &self.drm_key, boost).await,
            Ok(WriteOutcome::Wrote)
        ) {
            return true;
        }
        warn!("cannot re-apply the boost on {label}; resolving the window again");
        self.boosted_cgroup = None;
        self.current_unit.clear();
        false
    }

    async fn handle_focus(&mut self, cgroup: Option<String>, source: &str) -> bool {
        let Some(cgroup) = cgroup else {
            if self.boosted_cgroup.is_some() {
                info!("{source} has no app.slice unit; clearing the boost");
            }
            self.clear_boost().await;
            return false;
        };

        let label = unit_label(&cgroup).to_string();
        let boost = self.boost_bytes();

        // Same cgroup as last time. Trusting in-memory state would hide a
        // boost that something else reverted, so the file decides. Nothing is
        // cleared on this path: the cgroup keeping the boost is this one.
        if self.boosted_cgroup.as_deref() == Some(cgroup.as_str()) {
            if dmem_low_is(&cgroup, &self.drm_key, boost).await {
                return true;
            }
            info!("the boost on {label} was reverted from outside, applying it again");
        } else {
            self.clear_boost().await;
        }

        // Remembered before the write, not after: if the daemon is stopped
        // mid-write, its exit still knows which cgroup to clear.
        self.boosted_cgroup = Some(cgroup.clone());
        self.current_unit = label.clone();

        let boosted = match write_dmem_low(&cgroup, &self.drm_key, boost).await {
            Ok(WriteOutcome::Wrote) => {
                info!("boosted {label} to dmem.low={boost} ({source})");
                true
            }
            Ok(WriteOutcome::Missing) => {
                warn!(
                    "cannot boost {label}: it has no dmem.low. Is the user dmemcg-booster.service running?"
                );
                false
            }
            Ok(WriteOutcome::TimedOut) => {
                warn!("cannot boost {label}: the write to dmem.low did not finish in 2 s");
                false
            }
            Err(e) => {
                warn!("cannot boost {label}: {e}");
                false
            }
        };
        if !boosted {
            self.boosted_cgroup = None;
            self.current_unit.clear();
        }
        boosted
    }
}

/// Resolve and boost one window. A live pid is authoritative: it resolves
/// exactly through /proc, and a pid outside app.slice means "nothing to boost"
/// rather than a licence to guess. The app_id is matched against unit names
/// and process environments only when there is no usable pid (an X11 window
/// behind xwayland-satellite, or a process that exited meanwhile).
async fn apply_focus(inner: &Mutex<Inner>, scope: &Scope, pid: Option<u32>, app_id: &str) -> bool {
    // The lookup reads /proc, so it runs on the blocking pool, where a timeout
    // cannot cancel it. The deadline inside the closure is what actually stops
    // it; the outer timeout only covers the handoff.
    const BUDGET: Duration = Duration::from_millis(800);
    let deadline = Instant::now() + BUDGET;
    let app_slice = scope.app_slice.clone();
    let id = app_id.to_string();
    let lookup = tokio::time::timeout(
        BUDGET + Duration::from_millis(200),
        tokio::task::spawn_blocking(move || {
            let live = pid.filter(|p| cgroup_path_for_pid(*p).is_some());
            let comm = live.map(pid_comm).unwrap_or_default();
            match live {
                Some(p) => (find_app_scope_for_pid(p, 3, deadline), comm),
                None if !id.is_empty() => {
                    (find_app_scope_for_app_id(&app_slice, &id, deadline), comm)
                }
                None => (None, comm),
            }
        }),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let (cgroup, comm) = lookup.unwrap_or((None, String::new()));

    // A pid may belong to any user on the system; only this user's own slice
    // is this daemon's to write.
    let cgroup = cgroup.filter(|c| {
        let ours = std::path::Path::new(c).starts_with(&scope.user_root);
        if !ours {
            warn!(
                "ignoring {}: outside {}",
                loggable(c),
                scope.user_root.display()
            );
        }
        ours
    });

    let source = match pid {
        Some(p) => format!("pid={p} ({}) app_id={}", loggable(&comm), loggable(app_id)),
        None => format!("app_id={}", loggable(app_id)),
    };
    inner.lock().await.handle_focus(cgroup, &source).await
}

/// Hold `subscribe windows` open on the Umbriel socket and boost whatever is
/// active. Reconnects whenever the compositor is not there yet or goes away;
/// the initial snapshot after each (re)connect re-arms the boost. Waiting
/// backs off, so a session without Umbriel does not retry every three seconds
/// for hours.
async fn follow_umbriel(inner: Arc<Mutex<Inner>>, scope: Arc<Scope>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    const FIRST_RETRY: Duration = Duration::from_secs(3);
    const MAX_RETRY: Duration = Duration::from_secs(60);
    /// A `windows` snapshot is a few kilobytes. Reading further than this
    /// would grow the buffer without a bound the peer respects.
    const MAX_LINE: u64 = 1 << 20;

    let mut retry = FIRST_RETRY;
    let mut waiting = false;

    loop {
        let backoff = |retry: &mut Duration| {
            let wait = *retry;
            *retry = (*retry * 2).min(MAX_RETRY);
            wait
        };

        let Some(path) = umbriel_socket_path() else {
            if !waiting {
                warn!(
                    "no Umbriel socket (UMBRIEL_SOCKET / WAYLAND_DISPLAY unset, no umbriel-*.sock); waiting"
                );
                waiting = true;
            }
            tokio::time::sleep(backoff(&mut retry)).await;
            continue;
        };
        let mut stream = match tokio::net::UnixStream::connect(&path).await {
            Ok(s) => s,
            Err(e) => {
                if !waiting {
                    warn!(
                        "cannot connect to {}: {e}; waiting for Umbriel",
                        path.display()
                    );
                    waiting = true;
                }
                tokio::time::sleep(backoff(&mut retry)).await;
                continue;
            }
        };
        if let Err(e) = stream
            .write_all(b"{\"cmd\":\"subscribe\",\"events\":[\"windows\"]}\n")
            .await
        {
            warn!("subscribing to Umbriel failed: {e}");
            tokio::time::sleep(backoff(&mut retry)).await;
            continue;
        }
        info!("following {}", path.display());
        waiting = false;
        retry = FIRST_RETRY;
        inner.lock().await.following = path.display().to_string();
        // Fresh per connection: the boost was cleared when the last one went
        // away, so the first snapshot has to arm it again rather than be
        // recognised as the window that was already boosted.
        let mut tracker = Tracker::default();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            let read = {
                let mut limited = (&mut reader).take(MAX_LINE);
                limited.read_line(&mut line).await
            };
            match read {
                Ok(_) if line.is_empty() => break,
                Ok(_) if !line.ends_with('\n') => {
                    warn!(
                        "Umbriel sent more than {MAX_LINE} bytes without a newline; reconnecting"
                    );
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("reading from Umbriel failed: {e}");
                    break;
                }
            }
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(err) = v["err"].as_str() {
                tracing::error!(
                    "Umbriel rejected the subscription: {}. Retrying every {} s.",
                    loggable(err),
                    MAX_RETRY.as_secs()
                );
                retry = MAX_RETRY;
                break;
            }
            if v["event"] != "windows" {
                continue;
            }
            match tracker.next(&v["data"], Instant::now()) {
                Action::Boost { pid, app_id, key } => {
                    let boosted = apply_focus(&inner, &scope, pid, &app_id).await;
                    tracker.record(key, boosted, Instant::now());
                }
                Action::Verify { key } => {
                    if !inner.lock().await.verify_boost().await {
                        // the cgroup is gone: fall back to the retry path so
                        // the window is resolved again shortly
                        tracker.record(key, false, Instant::now());
                    }
                }
                Action::Clear => inner.lock().await.clear_boost().await,
                Action::Skip => {}
            }
        }

        info!("the Umbriel stream closed; clearing the boost");
        {
            let mut guard = inner.lock().await;
            guard.following.clear();
            guard.clear_boost().await;
        }
        tokio::time::sleep(backoff(&mut retry)).await;
    }
}

struct VramBoosterService {
    inner: Arc<Mutex<Inner>>,
    scope: Arc<Scope>,
}

#[interface(name = "org.umbriel.VramBooster")]
impl VramBoosterService {
    /// Manual entry point for debugging the resolution; the daemon feeds
    /// itself from the Umbriel socket. pid `-1` means "none".
    async fn focus_window(&self, pid: String, app_id: String) -> bool {
        let pid: Option<u32> = pid.trim().parse().ok().filter(|p| *p > 0);
        apply_focus(&self.inner, &self.scope, pid, app_id.trim()).await
    }

    async fn clear_focus(&self) -> bool {
        self.inner.lock().await.clear_boost().await;
        true
    }

    #[zbus(property)]
    async fn current_unit(&self) -> String {
        self.inner.lock().await.current_unit.clone()
    }

    #[zbus(property)]
    async fn following(&self) -> String {
        self.inner.lock().await.following.clone()
    }

    #[zbus(property)]
    async fn drm_key(&self) -> String {
        self.inner.lock().await.drm_key.clone()
    }

    #[zbus(property)]
    async fn vram_total(&self) -> u64 {
        self.inner.lock().await.vram_total
    }

    #[zbus(property)]
    async fn boost_ratio(&self) -> f64 {
        self.inner.lock().await.boost_ratio
    }

    #[zbus(property)]
    async fn boosted_bytes(&self) -> u64 {
        self.inner.lock().await.boost_bytes()
    }

    #[zbus(property)]
    async fn boosted_cgroup(&self) -> String {
        self.inner
            .lock()
            .await
            .boosted_cgroup
            .clone()
            .unwrap_or_default()
    }
}

fn die(message: &str) -> ! {
    tracing::error!("{message}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let boost_ratio = read_boost_ratio();
    let (drm_key, vram_total) = match read_dmem_capacity() {
        Ok(v) => v,
        Err(e) => die(&e),
    };
    let boost_bytes = (vram_total as f64 * boost_ratio) as u64;
    info!(
        "GPU {drm_key}, VRAM {} MiB, boost {boost_bytes} bytes ({:.0}% of it)",
        vram_total / 1024 / 1024,
        boost_ratio * 100.0
    );

    let Some(uid) = current_uid() else {
        die("cannot read /proc/self, so this session's uid is unknown");
    };
    let user_root = std::path::PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice"));
    let scope = Arc::new(Scope {
        app_slice: user_root.join(format!("user@{uid}.service/app.slice")),
        user_root,
    });

    let cleanup_key = drm_key.clone();
    let inner = Arc::new(Mutex::new(Inner {
        boosted_cgroup: None,
        current_unit: String::new(),
        following: String::new(),
        drm_key,
        vram_total,
        boost_ratio,
    }));

    // The bus name is claimed before anything is written: a second instance
    // has to fail here, while the running one still owns the boost it applied.
    let conn = connection::Builder::session()
        .and_then(|b| b.name("org.umbriel.VramBooster"))
        .and_then(|b| {
            b.serve_at(
                "/org/umbriel/VramBooster",
                VramBoosterService {
                    inner: inner.clone(),
                    scope: scope.clone(),
                },
            )
        });
    let _conn = match conn {
        Ok(builder) => match builder.build().await {
            Ok(c) => c,
            Err(zbus::Error::NameTaken) => {
                die("org.umbriel.VramBooster is already taken: another instance is running")
            }
            Err(e) => die(&format!(
                "cannot reach the session bus: {e}. Is $XDG_RUNTIME_DIR reachable? A sandbox setting that hides /run/user, such as ProtectHome=, looks like this."
            )),
        },
        Err(e) => die(&format!("cannot set up the session bus connection: {e}")),
    };

    let cleared = cleanup_stale_boosts(&scope.user_root, &cleanup_key, boost_bytes);
    if cleared > 0 {
        info!(
            "startup cleanup: cleared {cleared} stale boost value(s) under {}",
            scope.user_root.display()
        );
    }

    info!("ready on the session bus as org.umbriel.VramBooster");
    let follower = tokio::spawn(follow_umbriel(inner.clone(), scope.clone()));

    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => die(&format!("cannot listen for SIGTERM: {e}")),
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => die(&format!("cannot listen for SIGINT: {e}")),
    };

    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM"),
        _ = sigint.recv() => info!("received SIGINT"),
    }

    follower.abort();
    inner.lock().await.clear_boost().await;
    info!("cleanup done, exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_boost_ratio_bounds() {
        assert_eq!(parse_boost_ratio("0.85"), Some(0.85));
        assert_eq!(parse_boost_ratio("0"), Some(0.0));
        assert_eq!(parse_boost_ratio("1"), Some(1.0));
        assert_eq!(parse_boost_ratio("1.5"), None);
        assert_eq!(parse_boost_ratio("-0.1"), None);
        assert_eq!(parse_boost_ratio("abc"), None);
    }
}

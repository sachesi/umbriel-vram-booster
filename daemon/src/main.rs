use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};
use zbus::{connection, interface};

mod cgroup;
mod matcher;
mod umbriel;

use cgroup::{
    cgroup_path_for_pid, cleanup_stale_boosts, current_uid, find_app_scope_for_pid, pid_comm,
    read_dmem_capacity, unit_label, write_dmem_low,
};
use matcher::find_app_scope_for_app_id;
use umbriel::{pick_window, umbriel_socket_path};

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

struct Inner {
    prev_cgroup: Option<String>,
    current_unit: String,
    drm_key: String,
    vram_total: u64,
    boost_ratio: f64,
}

impl Inner {
    fn boost_bytes(&self) -> u64 {
        (self.vram_total as f64 * self.boost_ratio) as u64
    }

    async fn reset_previous(&mut self) {
        if let Some(ref prev) = self.prev_cgroup {
            match write_dmem_low(prev, &self.drm_key, 0).await {
                Ok(true) => info!("dmem.low=0 \u{2190} {}", unit_label(prev)),
                Ok(false) => info!("dmem.low missing (scope gone?): {}", unit_label(prev)),
                Err(e) => warn!(
                    "Failed to revert dmem.low to 0 for {}: {e}",
                    unit_label(prev)
                ),
            }
        }
        self.prev_cgroup = None;
        self.current_unit.clear();
    }

    async fn handle_focus(&mut self, cgroup: Option<String>, source: &str) -> bool {
        let cgroup = match cgroup {
            Some(p) => p,
            None => {
                info!("{source} skip (no app.slice unit); clearing previous boost");
                self.reset_previous().await;
                return false;
            }
        };

        // prev_cgroup is only ever set after a successful boost, so a match here
        // means the previous boost succeeded and remains in effect.
        if self.prev_cgroup.as_deref() == Some(cgroup.as_str()) {
            return true;
        }

        let label = unit_label(&cgroup).to_string();
        let boost = self.boost_bytes();
        info!("focus {source} \u{2192} dmem.low={boost} \u{2192} {label}");

        self.reset_previous().await;

        match write_dmem_low(&cgroup, &self.drm_key, boost).await {
            Ok(true) => {
                info!("dmem.low={boost} \u{2192} {label}");
                self.prev_cgroup = Some(cgroup);
                self.current_unit = label;
                true
            }
            Ok(false) => {
                warn!("Failed to boost {label}: dmem.low missing. Is dmemcg-booster running?");
                false
            }
            Err(e) => {
                warn!("Failed to write dmem.low boost for {label}: {e}");
                false
            }
        }
    }
}

/// Resolve and boost one window. A live pid is authoritative: it resolves
/// exactly through /proc, and a pid outside app.slice means "nothing to boost"
/// rather than a licence to guess. The app_id is matched against unit names
/// and process environments only when there is no usable pid (an X11 window
/// behind xwayland-satellite, or a process that exited meanwhile).
async fn apply_focus(
    inner: &Mutex<Inner>,
    app_slice: &std::path::Path,
    pid: Option<u32>,
    app_id: &str,
) -> bool {
    let root = app_slice.to_path_buf();
    let id = app_id.to_string();
    let lookup = tokio::time::timeout(
        std::time::Duration::from_millis(800),
        tokio::task::spawn_blocking(move || {
            let live = pid.filter(|p| cgroup_path_for_pid(*p).is_some());
            let comm = live.map(pid_comm).unwrap_or_default();
            match live {
                Some(p) => (find_app_scope_for_pid(p, 3), comm),
                None if !id.is_empty() => (find_app_scope_for_app_id(&root, &id), comm),
                None => (None, comm),
            }
        }),
    )
    .await
    .ok()
    .and_then(|r| r.ok());
    let (cgroup, comm) = lookup.unwrap_or((None, String::new()));
    let source = match pid {
        Some(p) => format!("pid={p} ({comm}) app_id={app_id}"),
        None => format!("app_id={app_id}"),
    };
    inner.lock().await.handle_focus(cgroup, &source).await
}

/// Hold `subscribe windows` open on the Umbriel socket and boost whatever is
/// active. Reconnects whenever the compositor is not there yet or goes away;
/// the initial snapshot after each (re)connect re-arms the boost.
async fn follow_umbriel(inner: Arc<Mutex<Inner>>, app_slice: Arc<std::path::PathBuf>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let retry = std::time::Duration::from_secs(3);
    let mut waiting = false;
    loop {
        let Some(path) = umbriel_socket_path() else {
            if !waiting {
                warn!(
                    "no Umbriel socket (UMBRIEL_SOCKET / WAYLAND_DISPLAY unset, no umbriel-*.sock); waiting"
                );
                waiting = true;
            }
            tokio::time::sleep(retry).await;
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
                tokio::time::sleep(retry).await;
                continue;
            }
        };
        if let Err(e) = stream
            .write_all(b"{\"cmd\":\"subscribe\",\"events\":[\"windows\"]}\n")
            .await
        {
            warn!("subscribe failed: {e}");
            tokio::time::sleep(retry).await;
            continue;
        }
        info!("following {}", path.display());
        waiting = false;
        let (reader, _writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        // Key of the last window that was boosted; a repeated snapshot for the
        // same window (title, geometry) costs nothing. A failed resolution is
        // not remembered, so the next event retries.
        let mut last: Option<String> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(err) = v["err"].as_str() {
                tracing::error!("Umbriel rejected the subscription: {err}");
                break;
            }
            if v["event"] != "windows" {
                continue;
            }
            match pick_window(&v["data"]) {
                Some((pid, app_id)) => {
                    let key = match pid {
                        Some(p) => format!("pid:{p}"),
                        None => format!("app:{app_id}"),
                    };
                    if last.as_deref() == Some(key.as_str()) {
                        continue;
                    }
                    let boosted = apply_focus(&inner, &app_slice, pid, &app_id).await;
                    last = boosted.then_some(key);
                }
                None => {
                    last = None;
                    inner.lock().await.reset_previous().await;
                }
            }
        }
        info!("Umbriel stream closed; clearing boost");
        inner.lock().await.reset_previous().await;
        tokio::time::sleep(retry).await;
    }
}

struct VramBoosterService {
    inner: Arc<Mutex<Inner>>,
    app_slice: Arc<std::path::PathBuf>,
}

#[interface(name = "org.umbriel.VramBooster")]
impl VramBoosterService {
    /// Manual entry point for debugging the resolution; the daemon feeds
    /// itself from the Umbriel socket. pid `-1` means "none".
    async fn focus_window(&self, pid: String, app_id: String) -> bool {
        let pid: Option<u32> = pid.trim().parse().ok().filter(|p| *p > 0);
        apply_focus(&self.inner, &self.app_slice, pid, app_id.trim()).await
    }

    async fn clear_focus(&self) -> bool {
        self.inner.lock().await.reset_previous().await;
        true
    }

    #[zbus(property)]
    async fn current_unit(&self) -> String {
        self.inner.lock().await.current_unit.clone()
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
    async fn prev_cgroup(&self) -> String {
        self.inner
            .lock()
            .await
            .prev_cgroup
            .clone()
            .unwrap_or_default()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let boost_ratio = read_boost_ratio();
    info!("boost_ratio={boost_ratio}");

    let (drm_key, vram_total) = match read_dmem_capacity() {
        Some(v) => v,
        None => {
            tracing::error!(
                "No dmem capacity in /sys/fs/cgroup/dmem.capacity. Is dmemcg-booster running?"
            );
            std::process::exit(1);
        }
    };
    let boost_bytes = (vram_total as f64 * boost_ratio) as u64;
    info!(
        "GPU: {drm_key}, VRAM: {vram_total} bytes ({} MiB), boost: {boost_bytes} bytes",
        vram_total / 1024 / 1024
    );

    let uid = current_uid();
    let cleanup_root =
        std::path::PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice"));
    let app_slice = Arc::new(cleanup_root.join(format!("user@{uid}.service/app.slice")));
    let cleared = cleanup_stale_boosts(&cleanup_root, &drm_key, boost_bytes);
    info!(
        "startup cleanup: cleared {cleared} stale dmem.low boost value(s) under {cleanup_root:?}"
    );

    let inner = Arc::new(Mutex::new(Inner {
        prev_cgroup: None,
        current_unit: String::new(),
        drm_key,
        vram_total,
        boost_ratio,
    }));

    let _conn = connection::Builder::session()?
        .name("org.umbriel.VramBooster")?
        .serve_at(
            "/org/umbriel/VramBooster",
            VramBoosterService {
                inner: inner.clone(),
                app_slice: app_slice.clone(),
            },
        )?
        .build()
        .await?;

    info!("umbriel-vram-booster ready on session bus (org.umbriel.VramBooster)");
    let follower = tokio::spawn(follow_umbriel(inner.clone(), app_slice.clone()));

    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM"),
        _ = sigint.recv() => info!("received SIGINT"),
    }

    follower.abort();
    let mut guard = inner.lock().await;
    guard.reset_previous().await;
    info!("cleanup done, exiting");
    Ok(())
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

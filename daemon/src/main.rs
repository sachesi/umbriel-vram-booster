use std::fs;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};
use zbus::{connection, interface};

fn parse_dmem_capacity(content: &str) -> Vec<(String, u64)> {
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

fn read_dmem_capacity() -> Option<(String, u64)> {
    let content = fs::read_to_string("/sys/fs/cgroup/dmem.capacity").ok()?;
    let entries: Vec<(String, u64)> = parse_dmem_capacity(&content);

    if let Ok(override_key) = std::env::var("DRM_KEY") {
        return entries
            .into_iter()
            .find(|(k, _)| *k == override_key)
            .or_else(|| {
                warn!("DRM_KEY={override_key} not found in dmem.capacity");
                None
            });
    }

    entries.into_iter().max_by_key(|(_, v)| *v)
}

fn cgroup_path_for_pid(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    for line in text.lines() {
        if let Some(rel) = line.strip_prefix("0::") {
            return Some(format!("/sys/fs/cgroup{}", rel.trim()));
        }
    }
    None
}

async fn write_dmem_low(cgroup_dir: &str, drm_key: &str, bytes: u64) -> std::io::Result<bool> {
    if cgroup_dir.contains("..") {
        return Ok(false);
    }
    let file = format!("{cgroup_dir}/dmem.low");
    let drm_key = drm_key.to_string();
    let cgroup_dir = cgroup_dir.to_string();
    match tokio::time::timeout(std::time::Duration::from_secs(2), async move {
        if tokio::fs::metadata(&file).await.is_err() {
            return Ok::<bool, std::io::Error>(false);
        }
        tokio::fs::write(&file, format!("{drm_key} {bytes}\n")).await?;
        Ok(true)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            warn!("write_dmem_low timed out for {cgroup_dir}");
            Ok(false)
        }
    }
}

fn is_app_scope(cgroup_dir: &str) -> bool {
    cgroup_dir.split('/').any(|c| c == "app.slice")
}

/// True if `content` (a dmem.low file body) sets `drm_key` to exactly `value`.
fn dmem_low_has_value(content: &str, drm_key: &str, value: u64) -> bool {
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
fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

/// Best-effort startup cleanup: clear dmem.low values left behind by a crashed
/// or SIGKILLed daemon. Only clears app.slice scopes whose value for the selected
/// drm_key equals our boost value; unrelated values are left untouched. Scoped to
/// `root` (our own user-<uid>.slice) to avoid touching other users' cgroups.
fn cleanup_stale_boosts(root: &std::path::Path, drm_key: &str, boost_bytes: u64) -> usize {
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

fn unit_label(cgroup_dir: &str) -> &str {
    cgroup_dir.rsplit('/').next().unwrap_or(cgroup_dir)
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn pid_comm(pid: u32) -> String {
    read_trimmed(&format!("/proc/{pid}/comm")).unwrap_or_default()
}

fn find_app_scope_for_pid(pid: u32, max_depth: usize) -> Option<String> {
    fn check(pid: u32, depth: usize, max_depth: usize) -> Option<String> {
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
                if let Some(cg) = check(child, depth + 1, max_depth) {
                    return Some(cg);
                }
            }
        }
        None
    }
    check(pid, 0, max_depth)
}

/// Undo systemd unit-name escaping (`\xNN` -> byte) so `app-code\x2doss@...`
/// compares as `code-oss`.
fn unescape_unit_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1] == b'x'
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 4])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Name tokens an app.slice unit core or a Wayland app_id can be compared by.
/// The whole string always counts. When it has several `-` parts, the first is
/// a launcher name (`flatpak-org.mozilla.firefox`, `leyen-<uuid>`) and is
/// skipped; every other part also yields its last dot segment and its `_`
/// pieces, so `steam_app_ly5550` gives `steam` and `ly5550`. Tokens shorter
/// than three characters and the bare `app` are dropped: they identify nothing.
fn name_tokens(core: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut push = |t: &str| {
        let t = t.to_ascii_lowercase();
        if t.len() >= 3 && t != "app" && !tokens.contains(&t) {
            tokens.push(t);
        }
    };
    push(core);
    let parts: Vec<&str> = core.split('-').filter(|p| !p.is_empty()).collect();
    let skip = usize::from(parts.len() > 1);
    for part in parts.into_iter().skip(skip) {
        push(part);
        if let Some(last) = part.rsplit('.').next()
            && last != part
        {
            push(last);
        }
        if part.contains('_') {
            for sub in part.split('_') {
                push(sub);
            }
        }
    }
    tokens
}

/// The game id a Proton/umu window advertises: `steam_app_<id>`.
fn steam_app_id(app_id: &str) -> Option<&str> {
    app_id
        .strip_prefix("steam_app_")
        .filter(|id| !id.is_empty())
}

/// True if a NUL-separated environ block carries `id` as a Steam/umu game id.
fn environ_has_game_id(environ: &[u8], id: &str) -> bool {
    environ.split(|b| *b == 0).any(|entry| {
        let Ok(entry) = std::str::from_utf8(entry) else {
            return false;
        };
        let Some((key, value)) = entry.split_once('=') else {
            return false;
        };
        matches!(
            key,
            "SteamAppId" | "SteamGameId" | "STEAM_COMPAT_APP_ID" | "GAMEID"
        ) && (value == id || value.strip_prefix("umu-") == Some(id))
    })
}

/// True if any process in the unit runs with `id` as its Steam/umu game id.
/// Links a leyen/umu scope named after an internal uuid to the window's
/// `steam_app_<id>` class.
fn unit_env_has_game_id(unit_dir: &std::path::Path, id: &str) -> bool {
    let Ok(procs) = fs::read_to_string(unit_dir.join("cgroup.procs")) else {
        return false;
    };
    procs.lines().take(256).any(|pid| {
        fs::read(format!("/proc/{pid}/environ"))
            .map(|env| environ_has_game_id(&env, id))
            .unwrap_or(false)
    })
}

/// Strip the systemd unit framing from a unit file name so only app-identifying
/// tokens remain: `app-<launcher>-<AppID>-<RANDOM>.scope`,
/// `app-<AppID>@<RANDOM>.service`, `dbus-:1.2-<Name>@0.service`, or any other
/// `<prefix>-<id>-<numbers>.scope` such as leyen's `leyen-<gameid>-<epoch>-<n>.scope`.
fn app_unit_core(unit: &str) -> Option<String> {
    let name = unescape_unit_name(unit);
    let stem = name
        .strip_suffix(".service")
        .or_else(|| name.strip_suffix(".scope"))?;
    let core = match stem.strip_prefix("app-") {
        Some(rest) => rest,
        None => match stem.strip_prefix("dbus-") {
            Some(rest) => rest.split_once('-').map(|(_, r)| r).unwrap_or(rest),
            None => stem,
        },
    };
    let mut core = core.split('@').next().unwrap_or(core);
    // Trailing numeric components are pids, epochs, nonces.
    while let Some((head, tail)) = core.rsplit_once('-')
        && !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        core = head;
    }
    Some(core.to_string())
}

/// True if the unit name carries a token that identifies `app_id`.
/// ponytail: name heuristic, only for X11 windows whose pid Umbriel cannot
/// report (xwayland-satellite); goes away if the bridge ever exposes _NET_WM_PID.
fn unit_matches_app_id(unit: &str, app_id: &str) -> bool {
    if app_id.is_empty() {
        return false;
    }
    let Some(core) = app_unit_core(unit) else {
        return false;
    };
    let wanted = name_tokens(app_id);
    name_tokens(&core).iter().any(|t| wanted.contains(t))
}

/// True if any process in the unit's cgroup.procs has a comm that identifies
/// `app_id`. Covers units whose name says nothing about the app: `run-*.scope`
/// from `systemd-run --scope`, and `dbus-:1.2-<Name>@0.service` from D-Bus
/// activation. comm is truncated to 15 bytes, so compare by prefix too.
fn unit_procs_match_app_id(unit_dir: &std::path::Path, app_id: &str) -> bool {
    let Ok(procs) = fs::read_to_string(unit_dir.join("cgroup.procs")) else {
        return false;
    };
    let wanted = name_tokens(app_id);
    procs.lines().take(64).any(|pid| {
        let comm = read_trimmed(&format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        !comm.is_empty()
            && wanted.iter().any(|t| {
                *t == comm || (comm.len() == 15 && t.len() > 15 && t.starts_with(comm.as_str()))
            })
    })
}

/// Leaf units (.scope / .service) under `app_slice`. D-Bus activated apps sit
/// one level down, in `app-dbus-…-<Name>.slice/dbus-…-<Name>@0.service`, so
/// sub-slices are descended into and never returned themselves.
fn app_slice_units(app_slice: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            if entry.file_name().to_string_lossy().ends_with(".slice") {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut units = Vec::new();
    walk(app_slice, &mut units);
    units.sort();
    units
}

/// Resolve a Wayland app_id to an app.slice unit cgroup: by unit name first,
/// then by the game id in the processes' environment, then by their comm.
fn find_app_scope_for_app_id(app_slice: &std::path::Path, app_id: &str) -> Option<String> {
    let units = app_slice_units(app_slice);
    if units.is_empty() {
        return None;
    }
    let by_name: Vec<&std::path::PathBuf> = units
        .iter()
        .filter(|p| {
            unit_matches_app_id(&p.file_name().unwrap_or_default().to_string_lossy(), app_id)
        })
        .collect();
    let by_env: Vec<&std::path::PathBuf> = match (by_name.is_empty(), steam_app_id(app_id)) {
        (true, Some(id)) => units
            .iter()
            .filter(|p| unit_env_has_game_id(p, id))
            .collect(),
        _ => Vec::new(),
    };
    let matches: Vec<&std::path::PathBuf> = if !by_name.is_empty() {
        by_name
    } else if !by_env.is_empty() {
        by_env
    } else {
        units
            .iter()
            .filter(|p| unit_procs_match_app_id(p, app_id))
            .collect()
    };
    if matches.len() > 1 {
        warn!(
            "app_id={app_id}: {} matching units, using {}",
            matches.len(),
            unit_label(&matches[0].to_string_lossy())
        );
    }
    matches.first().map(|p| p.to_string_lossy().into_owned())
}

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

/// Umbriel's IPC socket: `UMBRIEL_SOCKET`, else derived from `WAYLAND_DISPLAY`,
/// else the newest `umbriel-*.sock` in the runtime dir, since a systemd user
/// service does not always inherit the compositor's environment.
fn umbriel_socket_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("UMBRIEL_SOCKET") {
        return Some(p.into());
    }
    let run = std::env::var("XDG_RUNTIME_DIR").ok()?;
    if let Ok(display) = std::env::var("WAYLAND_DISPLAY") {
        return Some(format!("{run}/umbriel-{display}.sock").into());
    }
    fs::read_dir(&run)
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

/// The window to boost from one `windows` snapshot: the seat-global `active`
/// entry (`focused` is per workspace). Its pid counts only for a native
/// Wayland client; X11 windows report the xwayland-satellite bridge, or -1.
fn pick_window(windows: &serde_json::Value) -> Option<(Option<u32>, String)> {
    let active = windows.as_array()?.iter().find(|w| w["active"] == true)?;
    let pid = active["pid"]
        .as_i64()
        .filter(|p| *p > 0 && active["xwayland"] != true)
        .map(|p| p as u32);
    let app_id = active["app_id"].as_str().unwrap_or("").to_string();
    if pid.is_none() && app_id.is_empty() {
        return None;
    }
    Some((pid, app_id))
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
    fn parse_boost_ratio_bounds() {
        assert_eq!(parse_boost_ratio("0.85"), Some(0.85));
        assert_eq!(parse_boost_ratio("0"), Some(0.0));
        assert_eq!(parse_boost_ratio("1"), Some(1.0));
        assert_eq!(parse_boost_ratio("1.5"), None);
        assert_eq!(parse_boost_ratio("-0.1"), None);
        assert_eq!(parse_boost_ratio("abc"), None);
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

    #[test]
    fn unescape_unit_name_decodes_hex() {
        assert_eq!(unescape_unit_name("code\\x2doss"), "code-oss");
        assert_eq!(unescape_unit_name("plain"), "plain");
        assert_eq!(unescape_unit_name("bad\\x2"), "bad\\x2");
    }

    #[test]
    fn app_unit_core_strips_framing() {
        assert_eq!(
            app_unit_core("app-org.mozilla.firefox@1b2c.service").as_deref(),
            Some("org.mozilla.firefox")
        );
        assert_eq!(
            app_unit_core("app-gnome-org.mozilla.firefox-1234.scope").as_deref(),
            Some("gnome-org.mozilla.firefox")
        );
        assert_eq!(
            app_unit_core("app-code\\x2doss@x.service").as_deref(),
            Some("code-oss")
        );
        assert_eq!(
            app_unit_core("leyen-ly5550-1757000000-3.scope").as_deref(),
            Some("leyen-ly5550")
        );
        assert_eq!(
            app_unit_core("run-p13576-i46132.scope").as_deref(),
            Some("run-p13576-i46132")
        );
        assert_eq!(app_unit_core("app-foo.socket"), None);
    }

    #[test]
    fn unit_matches_app_id_by_token() {
        assert!(unit_matches_app_id(
            "app-org.mozilla.firefox@1b2c.service",
            "firefox"
        ));
        assert!(unit_matches_app_id(
            "app-firefox@1b2c.service",
            "org.mozilla.firefox"
        ));
        assert!(unit_matches_app_id(
            "app-gnome-org.mozilla.firefox-1234.scope",
            "Firefox"
        ));
        assert!(unit_matches_app_id(
            "app-code\\x2doss@x.service",
            "code-oss"
        ));
        assert!(unit_matches_app_id("app-steam@x.service", "steam_app_123"));
        assert!(unit_matches_app_id(
            "app-flatpak-org.mozilla.firefox-1126565164.scope",
            "org.mozilla.firefox"
        ));
        assert!(unit_matches_app_id(
            "dbus-:1.2-org.gnome.Loupe@0.service",
            "org.gnome.Loupe"
        ));
        assert!(!unit_matches_app_id(
            "app-steam@x.service",
            "org.kde.dolphin"
        ));
        assert!(!unit_matches_app_id("app-steam@x.service", ""));
        assert!(unit_matches_app_id("dbus.service", "dbus"));
    }

    #[test]
    fn app_slice_units_descends_into_slices() {
        let root = std::env::temp_dir().join(format!("uvb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app-flatpak-org.mozilla.firefox-1.scope")).unwrap();
        fs::create_dir_all(root.join(
            "app-dbus\\x2d:1.2\\x2dorg.gnome.Loupe.slice/dbus-:1.2-org.gnome.Loupe@0.service",
        ))
        .unwrap();
        fs::write(root.join("cgroup.procs"), "").unwrap();
        let units: Vec<String> = app_slice_units(&root)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            units,
            vec![
                "dbus-:1.2-org.gnome.Loupe@0.service".to_string(),
                "app-flatpak-org.mozilla.firefox-1.scope".to_string(),
            ]
        );
        assert_eq!(
            find_app_scope_for_app_id(&root, "org.gnome.Loupe").map(|p| unit_label(&p).to_string()),
            Some("dbus-:1.2-org.gnome.Loupe@0.service".to_string())
        );
        assert_eq!(
            find_app_scope_for_app_id(&root, "org.mozilla.firefox")
                .map(|p| unit_label(&p).to_string()),
            Some("app-flatpak-org.mozilla.firefox-1.scope".to_string())
        );
        assert_eq!(find_app_scope_for_app_id(&root, "Alacritty"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn environ_game_id_keys() {
        let env = b"HOME=/h\0GAMEID=umu-ly5550\0STEAM_COMPAT_APP_ID=ly5550\0";
        assert!(environ_has_game_id(env, "ly5550"));
        assert!(!environ_has_game_id(env, "ly5551"));
        assert!(!environ_has_game_id(b"PATH=/bin\0", "ly5550"));
        assert_eq!(steam_app_id("steam_app_ly5550"), Some("ly5550"));
        assert_eq!(steam_app_id("steam_app_"), None);
        assert_eq!(steam_app_id("firefox"), None);
    }

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
}

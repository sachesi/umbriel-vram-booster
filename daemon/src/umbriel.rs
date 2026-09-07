//! Umbriel's IPC socket: where it lives and what to take from a `windows`
//! snapshot.

use std::fs;

/// Umbriel's IPC socket: `UMBRIEL_SOCKET`, else derived from `WAYLAND_DISPLAY`,
/// else the newest `umbriel-*.sock` in the runtime dir, since a systemd user
/// service does not always inherit the compositor's environment.
pub(crate) fn umbriel_socket_path() -> Option<std::path::PathBuf> {
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
pub(crate) fn pick_window(windows: &serde_json::Value) -> Option<(Option<u32>, String)> {
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
}

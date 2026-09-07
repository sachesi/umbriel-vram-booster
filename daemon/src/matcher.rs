//! Resolving a Wayland `app_id` to a unit under `app.slice`, for windows
//! whose pid Umbriel cannot report (X11 through xwayland-satellite).

use std::fs;
use tracing::warn;

use crate::cgroup::{read_trimmed, unit_label};

/// Undo systemd unit-name escaping (`\xNN` -> byte) so `app-code\x2doss@...`
/// compares as `code-oss`.
pub(crate) fn unescape_unit_name(name: &str) -> String {
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
pub(crate) fn name_tokens(core: &str) -> Vec<String> {
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
pub(crate) fn steam_app_id(app_id: &str) -> Option<&str> {
    app_id
        .strip_prefix("steam_app_")
        .filter(|id| !id.is_empty())
}

/// True if a NUL-separated environ block carries `id` as a Steam/umu game id.
pub(crate) fn environ_has_game_id(environ: &[u8], id: &str) -> bool {
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
pub(crate) fn unit_env_has_game_id(unit_dir: &std::path::Path, id: &str) -> bool {
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
pub(crate) fn app_unit_core(unit: &str) -> Option<String> {
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
pub(crate) fn unit_matches_app_id(unit: &str, app_id: &str) -> bool {
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
pub(crate) fn unit_procs_match_app_id(unit_dir: &std::path::Path, app_id: &str) -> bool {
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
pub(crate) fn app_slice_units(app_slice: &std::path::Path) -> Vec<std::path::PathBuf> {
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
pub(crate) fn find_app_scope_for_app_id(
    app_slice: &std::path::Path,
    app_id: &str,
) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

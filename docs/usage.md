# Usage

## How it works

The focused window receives VRAM priority (`dmem.low` set to VRAM × boost_ratio, default 90%). All other apps are set to zero. When focus switches, the previous app is reverted and the new one is boosted. Only apps with a systemd unit under `app.slice` can be boosted — see below.

The boost ratio prevents starving the compositor and other GPU consumers. Override via the `VRAM_BOOST_RATIO` environment variable in the systemd user service:

```
systemctl --user edit umbriel-vram-booster.service
```

Add:

```
[Service]
Environment=VRAM_BOOST_RATIO=0.85
```

Then `systemctl --user restart umbriel-vram-booster.service`.

## Daemon status

```
umbriel-vram-boosterctl
```

Example output:

```
=== Umbriel VRAM Booster Status ===
Daemon:           running
DRM key:          drm/0000:2d:00.0/vram
VRAM total:       8573157376 (8176 MiB, 7.98 GiB)
Boost ratio:      90%
Boosted bytes:    7715841638 (7360 MiB, 7.19 GiB) (90% of total)
Current unit:     app-flatpak-org.mozilla.firefox-1126565164.scope
Boosted cgroup:   (none)
```

Running `umbriel-vram-boosterctl` from a terminal shows the terminal's own state, because the terminal is the active window. Focus the app you care about first, e.g. `sleep 5; umbriel-vram-boosterctl` and click over within 5 s.

## Verifying

Watch `dmem.low` values update as you switch focus between apps:

```
find /sys/fs/cgroup/user.slice -name "dmem.low" -path "*/app.slice/*" \
  | xargs grep -v " 0$" 2>/dev/null
```

The focused app's unit should show 90% of VRAM capacity. All others should be zero.

Follow the daemon log (`RUST_LOG=info` in the unit shows every focus change and which socket it follows):

```
just logs
# or: journalctl --user -u umbriel-vram-booster.service -f
```

See what Umbriel reports as active, and what the daemon gets to work with:

```
umbriel windows --json | jq '.[] | select(.active) | {app_id, pid, xwayland}'
```

Drive the resolution by hand (`-1` means no pid):

```
busctl --user call org.umbriel.VramBooster /org/umbriel/VramBooster org.umbriel.VramBooster FocusWindow ss -1 firefox
busctl --user call org.umbriel.VramBooster /org/umbriel/VramBooster org.umbriel.VramBooster FocusWindow ss $(pgrep -n firefox) firefox
busctl --user call org.umbriel.VramBooster /org/umbriel/VramBooster org.umbriel.VramBooster ClearFocus
```

The next Umbriel event overrides whatever you set by hand.

## Apps must live in `app.slice`

The daemon can only boost a process that has its own systemd unit under `app.slice`. Check:

```
systemctl --user list-units '*.scope' 'app-*' --no-legend
```

If the focused app is not listed, pick one of:

**1. Let Noctalia launch apps as systemd services (preferred with Noctalia)**

Noctalia's default is `launch_apps_as_systemd_services = false`, so its launcher, dock and taskbar start apps as plain children of the shell. Settings → Shell → General → *Launch apps as systemd services*, or in the config:

```toml
launch_apps_as_systemd_services = true
```

Apps then run as `app-<desktop-id>@<uuid>.service`. This only works when Noctalia itself runs under the systemd user manager (as a user unit, or started via uwsm); otherwise Noctalia ignores the option and greys out the toggle.

**2. Wrap the launch in `systemd-run`**

For a terminal, an Umbriel `exec` keybind, or Noctalia's `launch_apps_custom_command`:

```
systemd-run --user --scope --slice=app.slice -- leyen run {game_id}
```

```toml
launch_apps_custom_command = "systemd-run --user --scope --slice=app.slice -- $CMD"
```

**How a window is matched to a unit**

A native Wayland window carries its client pid, which resolves through `/proc/<pid>/cgroup`; that verdict is final. An X11 window (through xwayland-satellite) reports pid `-1`, so its app id is used: by unit name first (the app id, its last dot segment such as `firefox` in `org.mozilla.firefox`, and its underscore parts such as `steam`, `ly5550` in `steam_app_ly5550`, against the tokens of every unit name, whatever its prefix; the first `-` part of a multi-part name is a launcher and never counts, nor do tokens under three characters or `app`). Second, for a `steam_app_<id>` window with no name match, every unit's processes are checked for `<id>` in `SteamAppId`, `SteamGameId`, `STEAM_COMPAT_APP_ID` or `GAMEID` (with or without `umu-`), which links a umu/leyen scope named after an internal uuid to its window. Third, `comm`. D-Bus activated apps (`dbus-:1.2-<Name>@0.service` inside an `app-dbus-…-<Name>.slice`) are found too. If a running app is still skipped, compare `umbriel windows --json` with `systemctl --user list-units` and `cat /proc/<pid>/comm`.

**Single-instance apps (Zed, VS Code, etc.)**

Some editors use a daemon model — the first launch starts a background process; all subsequent launches attach to it. If that daemon was ever started from a terminal, it lives outside `app.slice` and will be skipped even when you click the app in the launcher. Kill it and relaunch from the launcher.

## Troubleshooting

**Daemon not following focus**

```
journalctl --user -u umbriel-vram-booster -b --no-pager | tail
```

`no Umbriel socket` means neither `UMBRIEL_SOCKET` nor `WAYLAND_DISPLAY` reached the user manager and no `umbriel-*.sock` exists in `$XDG_RUNTIME_DIR`. `cannot connect` means the socket path exists but Umbriel is not listening. Both retry every 3 s. To pin the socket:

```
systemctl --user edit umbriel-vram-booster.service
# [Service]
# Environment=UMBRIEL_SOCKET=/run/user/1000/umbriel-wayland-1.sock
```

**Daemon fails to start**

Common cause: `dmemcg-booster` is not running, or `dmem` is not in `cgroup.controllers`.

**"Failed to boost ... dmem.low missing"**

The user `dmemcg-booster.service` has not propagated the controller into app units. Check `systemctl --user status dmemcg-booster.service`.

**"Permission denied" writing dmem.low**

The daemon runs as your user and relies on `dmem.low` under your `user@<uid>.service` subtree being user-writable (set up by the user `dmemcg-booster.service`). If your setup leaves those files root-owned, the user-session model cannot write them — check the `dmemcg-booster` configuration.

**Several units match one app id**

The daemon logs `N matching units, using <unit>` and boosts the first by name. Two running X11 instances of the same app are indistinguishable by name; close one.

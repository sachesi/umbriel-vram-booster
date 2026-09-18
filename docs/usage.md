# Usage

## How it works

The focused window receives VRAM priority (`dmem.low` set to VRAM × boost_ratio, default 90%). All other apps are set to zero. When focus switches, the previous app is reverted and the new one is boosted. Only apps with a systemd unit under `app.slice` can be boosted — see below.

The daemon connects to Umbriel's socket (`UMBRIEL_SOCKET`, else `$XDG_RUNTIME_DIR/umbriel-$WAYLAND_DISPLAY.sock` if it exists, else the newest `umbriel-*.sock` there) and subscribes to `windows`. From every snapshot it takes the `active` window, which is keyboard focus across the seat; `focused` is per workspace. When Umbriel restarts, the boost is dropped, and the daemon reconnects and applies it again from the first snapshot. On SIGTERM or SIGINT it drops the boost and exits.

The daemon also protects `session.slice`, and can put a ceiling on `app.slice` as a whole; see [below](#protecting-the-compositor).

The GPU is the largest `drm/` entry in `/sys/fs/cgroup/dmem.capacity`; set `DRM_KEY` in the unit to pick another one.

If the daemon is killed without cleaning up (`SIGKILL`, a crash), a boost can be left behind. At startup it clears every `dmem.low` under `app.slice` in its own `user-<uid>.slice` that holds exactly its own boost value for the selected GPU; any other value is left alone.

A ratio of 0.90 is aggressive: it leaves little headroom for the compositor and other GPU users. Lower it (0.80, say) if the compositor stutters or background apps are evicted. Override it in the systemd user service, not by running a second copy of the daemon by hand: the running instance owns the bus name, so a second one exits without doing anything.

```
systemctl --user edit umbriel-vram-booster.service
```

Add:

```
[Service]
Environment=VRAM_BOOST_RATIO=0.85
```

Then `systemctl --user restart umbriel-vram-booster.service`.

## Protecting the compositor

`dmemcg-booster` sets `dmem.low` on `app.slice` to the whole of VRAM, and nothing on its sibling `session.slice`. The kernel weighs protection between siblings, so next to `app.slice`, everything in `session.slice` is unprotected: once VRAM is full, any app's buffers, background apps' included, can push out a compositor running there (under uwsm, for one). The daemon therefore sets `session.slice`'s `dmem.low` to the whole of VRAM as well, which puts the compositor on a par with `app.slice`: the focused app evicts background apps' buffers first, and the compositor's only when nothing unprotected is left and a buffer moves back into VRAM. The rest of `session.slice` (portals, the notification daemon, Xwayland) is covered too; it holds little VRAM.

A compositor started in a login session scope (`session-N.scope`) already has this from the system `dmemcg-booster`, and one inside `app.slice` gets nothing from it. `VRAM_PROTECT_SESSION=0` turns it off:

```
[Service]
Environment=VRAM_PROTECT_SESSION=0
```

The daemon writes it only where `session.slice`'s `dmem.low` is 0, leaves a value someone else set alone, and puts 0 back at exit while the value is still its own, like the ceiling below.

## The ceiling on `app.slice`

Off by default, and mostly not needed with `session.slice` protected. It keeps VRAM for everything outside `app.slice` in a harder way: `VRAM_RESERVE_MIB` sets `app.slice`'s `dmem.max` to VRAM less that many MiB, which then stay with the rest:

```
[Service]
Environment=VRAM_RESERVE_MIB=256
```

It costs the focused app. A new buffer that would take `app.slice` past the ceiling goes straight to system memory (GTT) without evicting anything, even while background apps hold VRAM the focused app could otherwise take from them: from Linux 7.3 the kernel evicts for a protected allocation when VRAM itself is full, but not when a cgroup limit is hit. Only a buffer moved back into VRAM later evicts, inside `app.slice`, where the focused unit's `dmem.low` still protects it. Up to 7.2 new buffers never evict, and the ceiling makes `app.slice`'s share of VRAM smaller by the reserve. Turn it on if the compositor stutters when VRAM runs out, and compare with it off.

A reserve as large as the VRAM stops the daemon from starting. Nothing outside `app.slice` is limited: a compositor that itself runs inside `app.slice` gets nothing from the ceiling.

The daemon writes the ceiling at startup and checks it at every focus change, since `app.slice` is made anew when the user manager restarts. Up to Linux 7.2, the kernel keeps the old limit without an error if `app.slice` already uses more than the ceiling; the daemon notices and tries again at the next focus change. From 7.3 it applies at once. The daemon never evicts to make room: the write is non-blocking.

A `dmem.max` on `app.slice` that the daemon did not write is left alone, with a warning. On exit it restores `max`, if the ceiling there is still its own. A daemon killed outright leaves its ceiling behind; the next start takes it over, unless `VRAM_RESERVE_MIB` changed in between, in which case logging out and in clears it.

```
cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/app.slice/dmem.max
```

## Daemon status

```
umbriel-vram-boosterctl
```

Example output:

```
=== Umbriel VRAM Booster Status ===
Daemon:           running
Following:        /run/user/1000/umbriel-wayland-1.sock
DRM key:          drm/0000:2d:00.0/vram
VRAM total:       8573157376 (8176 MiB, 7.98 GiB)
Boost ratio:      90%
Boosted bytes:    7715841638 (7358 MiB, 7.19 GiB)
Session low:      8573157376 (8176 MiB, 7.98 GiB)
App ceiling:      off
Current unit:     app-flatpak-org.mozilla.firefox-1126565164.scope
Boosted cgroup:   /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.mozilla.firefox-1126565164.scope
```

`Following` is the Umbriel socket the daemon is reading. `(not connected -
waiting for Umbriel)` means it has none yet: the compositor is not up, or its
socket is not where the daemon looks (see Troubleshooting). `Current unit` and
`Boosted cgroup` are the same cgroup, by unit name and by full path; both are
`(none)` when nothing is boosted.

`--help` and `--version` are the only arguments it takes.

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
busctl --user call org.umbriel.VramBooster /org/umbriel/VramBooster org.umbriel.VramBooster FocusWindow ss -- -1 firefox
busctl --user call org.umbriel.VramBooster /org/umbriel/VramBooster org.umbriel.VramBooster FocusWindow ss $(pgrep -n firefox) firefox
busctl --user call org.umbriel.VramBooster /org/umbriel/VramBooster org.umbriel.VramBooster ClearFocus
```

The next `windows` event from Umbriel (a focus or title change) resolves the active window again and replaces whatever you set by hand.

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

A native Wayland window carries its client pid, which resolves through `/proc/<pid>/cgroup`; that verdict is final. An X11 window (through xwayland-satellite) reports pid `-1`, so its app id is used. First, for a `steam_app_<id>` window, every unit's processes are checked for `<id>` in `SteamAppId`, `SteamGameId`, `STEAM_COMPAT_APP_ID` or `GAMEID` (with or without `umu-`), which links a umu/leyen scope named after an internal uuid to its window; this comes before the name because the window shares the token `steam` with the Steam client's own unit. Second, by unit name (the app id, its last dot segment such as `firefox` in `org.mozilla.firefox`, and its underscore parts such as `steam`, `ly5550` in `steam_app_ly5550`, against the tokens of every unit name, whatever its prefix; the first `-` part of a multi-part name is a launcher and never counts, nor do tokens under three characters or `app`). Third, `comm`. D-Bus activated apps (`dbus-:1.2-<Name>@0.service` inside an `app-dbus-…-<Name>.slice`) are found too. If a running app is still skipped, compare `umbriel windows --json` with `systemctl --user list-units` and `cat /proc/<pid>/comm`.

**Single-instance apps (Zed, VS Code, etc.)**

Some editors use a daemon model — the first launch starts a background process; all subsequent launches attach to it. If that daemon was ever started from a terminal, it lives outside `app.slice` and will be skipped even when you click the app in the launcher. Kill it and relaunch from the launcher.

## Troubleshooting

**Daemon not following focus**

Run `umbriel-vram-boosterctl` first: if `Following` shows a socket, the daemon is connected and the problem is the app, not the connection. Otherwise:

```
journalctl --user -u umbriel-vram-booster -b --no-pager | tail
```

`no Umbriel socket` means `UMBRIEL_SOCKET` did not reach the user manager and no `umbriel-*.sock` exists in `$XDG_RUNTIME_DIR` (a `WAYLAND_DISPLAY` whose derived socket is missing falls back to that scan). `cannot connect` means the socket path exists but Umbriel is not listening. Both retry, starting at 3 s and backing off to 60 s, so a session that starts Umbriel late may take up to a minute to be picked up. To pin the socket:

```
systemctl --user edit umbriel-vram-booster.service
# [Service]
# Environment=UMBRIEL_SOCKET=/run/user/1000/umbriel-wayland-1.sock
```

**Daemon fails to start**

Common cause: `dmemcg-booster` is not running, or `dmem` is not in `cgroup.controllers`. The journal names the reason; a `VRAM_BOOST_RATIO` that is not a number above 0 and at most 1, a `VRAM_PROTECT_SESSION` other than 0 or 1, or a `VRAM_RESERVE_MIB` that is not a whole number below the VRAM size, stops it too.

**"Failed to boost ... dmem.low missing"**

The user `dmemcg-booster.service` has not propagated the controller into app units. Check `systemctl --user status dmemcg-booster.service`.

**"app.slice already has dmem.max=..."**

Something else, such as a oneshot service from a VRAM tuning guide, set a limit on `app.slice`. The daemon leaves it; remove the other setup, or leave `VRAM_RESERVE_MIB` unset to keep it.

**"Permission denied" writing dmem.low**

The daemon runs as your user and relies on `dmem.low` under your `user@<uid>.service` subtree being user-writable (set up by the user `dmemcg-booster.service`). If your setup leaves those files root-owned, the user-session model cannot write them — check the `dmemcg-booster` configuration.

**Several units match one app id**

The daemon logs `N matching units, using <unit>` and boosts the first by name. Two running X11 instances of the same app are indistinguishable by name; close one.

# umbriel-vram-booster

> **Experimental.** This is a local desktop tool under active development. It writes to privileged cgroup files and is not production-hardened. Test on a non-critical setup first.

Dynamic VRAM prioritization for the [Umbriel](https://github.com/noctalia-dev/umbriel) compositor via Linux dmem cgroups. Keeps the focused app's GPU memory protected from TTM eviction on 8 GB and under GPUs. The Umbriel sibling of [`gnome-vram-booster`](../gnome-vram-booster) and [`plasma-vram-booster`](../plasma-vram-booster).

One systemd `--user` daemon. It follows Umbriel's IPC socket directly. No root at runtime.

## Requirements

- Kernel 6.12+ with the `dmem` cgroup controller
- [`dmemcg-booster`](https://pixelcluster.github.io/VRAM-Mgmt-fixed/) — both the **system** service (propagates dmem into user session cgroups) and the **user** service (propagates dmem into app scopes) must be active
- Umbriel with `pid` in its `windows` IPC payload (merged in [#166](https://github.com/noctalia-dev/umbriel/pull/166)); older builds still work through the app id fallback
- AMD GPU (`amdgpu` driver)
- [`just`](https://github.com/casey/just) and a Rust toolchain to build/install

## Hardware / desktop support

| Category | Status |
|---|---|
| AMD GPU (`amdgpu`), Mesa/RADV | Target, supported |
| NVIDIA proprietary driver | Untested, likely unsupported (no dmemcg) |
| Intel GPU | Untested |
| Umbriel | Primary target; the shell on top (Noctalia or anything else) does not matter |
| Other compositors | Unsupported — the daemon speaks Umbriel's IPC |

## Install

```
just build    # needs a Rust toolchain; skip if daemon/target/release/ is already built
just install
```

See [docs/install.md](docs/install.md) for full instructions and [docs/usage.md](docs/usage.md) for usage and troubleshooting.

## How it works

1. The daemon connects to Umbriel's socket (`UMBRIEL_SOCKET`, or `$XDG_RUNTIME_DIR/umbriel-$WAYLAND_DISPLAY.sock`, or the newest `umbriel-*.sock` there) and sends `{"cmd":"subscribe","events":["windows"]}`. Umbriel answers with the current window list and then a fresh list on every change.
2. From each list it takes the `active` entry (seat-global keyboard focus; `focused` is per workspace).
3. A native Wayland window carries its client `pid`: the daemon resolves it to its systemd cgroup through `/proc`. An X11 window reports `-1` (its client is the xwayland-satellite bridge), so its `app_id` is matched against units under `app.slice` instead.
4. The daemon writes `dmem.low = VRAM_total * boost_ratio` to that unit's cgroup.
5. The previously boosted cgroup is reverted to 0.
6. When the socket closes (Umbriel restarts) or the daemon exits (SIGTERM/SIGINT), the boost is reset; the daemon reconnects and re-arms from the initial snapshot.

**app_id matching (X11 windows).** By unit name first, on any unit under `app.slice`: the app id, its last dot segment (`firefox` in `org.mozilla.firefox`) and its underscore parts (`steam`, `ly5550` in `steam_app_ly5550`) against the tokens of `app-flatpak-org.mozilla.firefox-….scope`, `dbus-:1.2-org.gnome.Loupe@0.service`, `leyen-ly5550-….scope` and the like. A `steam_app_<id>` window with no name match is linked through `SteamAppId` / `STEAM_COMPAT_APP_ID` / `GAMEID` in each unit's process environment, which is how umu and leyen scopes named after internal ids resolve. Last resort is the `comm` of the processes in each unit. If several units match, the first by name is used and a warning is logged.

**Apps must have an `app.slice` unit.** A pid or app id that resolves to nothing under `app.slice` is skipped and any previous boost is cleared. Noctalia's default `launch_apps_as_systemd_services = false` gives launcher apps no unit; enable it (needs Noctalia under the systemd user manager or uwsm), or wrap launches in `systemd-run --user --scope --slice=app.slice`. See [docs/usage.md](docs/usage.md).

**Startup cleanup.** If the daemon was killed (`SIGKILL`, crash) without running its exit cleanup, a stale `dmem.low` boost can persist. On startup the daemon scans its own `user-<uid>.slice` and clears only `dmem.low` entries whose value for the selected GPU equals its own boost value; unrelated values are left untouched.

Boost ratio defaults to 0.90 (90% of VRAM), which is **aggressive** — it leaves little headroom for the compositor and other GPU consumers. Lower it (e.g. 0.80) if you see compositor stutter or eviction of background apps. Override via `VRAM_BOOST_RATIO`:

```
VRAM_BOOST_RATIO=0.80 umbriel-vram-booster
```

Query daemon status:

```
umbriel-vram-boosterctl
```

## License

GPL-3.0-or-later

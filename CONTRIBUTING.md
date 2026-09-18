# Contributing

Bugs and ideas go to the [issue tracker](https://github.com/sachesi/umbriel-vram-booster/issues);
security problems do not, see [SECURITY.md](SECURITY.md).

Before a change goes in:

- `just check` and `just test` pass. CI runs both on Fedora 44, with `cargo deny check`,
  for every push and pull request.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  `fix:`, `feat:`, `docs:`, `build:` and so on, scoped where it helps (`fix(daemon):`,
  `fix(ctl):`, `fix(packaging):`), with a subject that says what changed.
- Behaviour described in the README and `docs/` changes with the code that implements it.

## Where things are

```
daemon/src/main.rs      the daemon: state, the D-Bus service, following the Umbriel
                        socket, startup and exit
daemon/src/umbriel.rs   where the socket is, reading its stream, which window a snapshot
                        makes active, and the Tracker that decides what each snapshot
                        is worth acting on
daemon/src/cgroup.rs    dmem.capacity, dmem.low and dmem.max, pid to cgroup through /proc,
                        startup cleanup of stale boosts
daemon/src/matcher.rs   app_id to an app.slice unit, for X11 windows without a pid
daemon/src/ctl.rs       umbriel-vram-boosterctl, reads the daemon's D-Bus properties
data/                   the systemd user unit
docs/                   installing, using and troubleshooting
```

The daemon writes only below its own `user-<uid>.slice`: `dmem.low` of app units, and
`dmem.max` of `app.slice`. What it
reads from a window or another process (app ids, unit names, `comm`, environments) is
untrusted: app ids, unit names and `comm` go through `loggable()` before they reach a
log line, and resolving a focused window is bounded by a deadline, since it walks `/proc`
and `app.slice`.

## Running

```
just build           # release build
just check           # rustfmt, clippy -D warnings
just test            # the unit tests
just reload          # reinstall the built binaries and restart the user service
just logs            # follow the daemon's journal
cargo deny --manifest-path daemon/Cargo.toml check
```

`RUST_LOG=info` shows every focus change. The D-Bus method `FocusWindow` resolves and
boosts a window by hand, which helps when a unit is not matched:

```
busctl --user call org.umbriel.VramBooster /org/umbriel/VramBooster \
    org.umbriel.VramBooster FocusWindow ss -- -1 steam_app_440
```

## Releases

A release bumps the version in `daemon/Cargo.toml` (and `Cargo.lock`) and is tagged
`v<version>`.

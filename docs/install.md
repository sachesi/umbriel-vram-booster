# Installation

## Prerequisites

**1. Kernel with dmem cgroup controller**

Verify support:

```
cat /sys/fs/cgroup/cgroup.controllers
```

The output must include `dmem`. Requires kernel 6.12 or newer. Distributions known to ship it: CachyOS, Nobara, Bazzite.

**2. dmemcg-booster**

This daemon propagates the dmem controller into user session cgroups. Without it, `dmem.low` files will not exist under app units and the booster cannot write to them.

Install from your distribution's repository or build from source:
https://pixelcluster.github.io/VRAM-Mgmt-fixed/

Enable and start both the system service (propagates dmem into user session cgroups) and the user service (propagates dmem into app scopes):

```
sudo systemctl enable --now dmemcg-booster.service
systemctl --user enable --now dmemcg-booster.service
```

**3. Umbriel**

The daemon speaks Umbriel's IPC socket. A build that reports `pid` in `umbriel windows --json` (PR #166) resolves native Wayland windows exactly; an older build falls back to app id matching for everything.

**4. Rust toolchain and just — on the build machine only**

Rust 1.88 or newer (the code uses let-chains). The machine you install on needs
neither: `just install` copies binaries and never compiles.

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Build and install

```
just build
just install
```

`just build` runs `cargo build --release` and needs the Rust toolchain. `just install` never builds: it only checks that `daemon/target/release/` holds the binaries, so a machine without Rust can install binaries built elsewhere. `just install` will:

- Install the binaries to `/usr/bin/umbriel-vram-booster` and `/usr/bin/umbriel-vram-boosterctl` (needs `sudo`)
- Install the systemd **user** service to `/usr/lib/systemd/user/` (needs `sudo`)
- Enable and start the user service (`systemctl --user enable --now`) — no root

The daemon finds Umbriel's socket by itself and waits for it if the compositor is not up yet, so the unit is wanted by `default.target` and needs no session hook.

## Uninstall

```
just uninstall
```

## Updating after code changes

```
just build
just reload
```

Reinstalls the binaries from `daemon/target/release/` and restarts the user service.

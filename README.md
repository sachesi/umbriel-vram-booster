# umbriel-vram-booster

Keeps the focused window's VRAM from being evicted on the [Umbriel](https://github.com/noctalia-dev/umbriel) compositor. A systemd `--user` daemon follows Umbriel's IPC socket and, through the Linux dmem cgroup controller, protects most of the VRAM for the active window's unit, taking the protection back from the window focused before, and protects `session.slice`, where the compositor usually runs, from background apps. It matters most on GPUs with 8 GB or less, where whatever runs in the background can otherwise push the foreground app's buffers out to system memory.

It needs no root at runtime. It is the Umbriel counterpart of [gnome-vram-booster](https://github.com/sachesi/gnome-vram-booster), and experimental: it writes cgroup files, so try it on a setup you can afford to reboot.

## Requirements

- Linux 6.12 or newer with the `dmem` cgroup controller.
- [dmemcg-booster](https://pixelcluster.github.io/VRAM-Mgmt-fixed/), both its system and its user service.
- Umbriel. A build that reports `pid` in its `windows` events ([#166](https://github.com/noctalia-dev/umbriel/pull/166)) resolves Wayland windows exactly; an older one falls back to matching app ids.
- An AMD GPU on `amdgpu`. Intel is untested; NVIDIA's proprietary driver is untested and likely lacks dmem support.
- Apps launched into a unit of their own under `app.slice`; see [docs/usage.md](docs/usage.md#apps-must-live-in-appslice).

## Building and installing

```
just build      # needs Rust 1.88
just install    # installs the binaries and enables the user service
```

`just install` never builds, so it can run on a machine without a Rust toolchain. Details and removal are in [docs/install.md](docs/install.md).

## Usage

```
umbriel-vram-boosterctl
```

prints the GPU, the boost size, the Umbriel socket the daemon follows and the unit that holds the boost. The boost is 90% of VRAM; lower it if the compositor stutters:

```
systemctl --user edit umbriel-vram-booster.service   # [Service] Environment=VRAM_BOOST_RATIO=0.80
```

`VRAM_RESERVE_MIB` caps `app.slice` that far below the VRAM size, keeping it for the compositor at a cost to the focused app; it is off by default. See [docs/usage.md](docs/usage.md#the-ceiling-on-appslice).

## Documentation

- [Installing](docs/install.md)
- [Usage](docs/usage.md), including how a window is matched to a unit, and troubleshooting
- [Contributing](CONTRIBUTING.md), including where things are in the code, and [reporting a vulnerability](SECURITY.md)

GPL-3.0-or-later.

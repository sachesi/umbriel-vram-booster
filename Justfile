# umbriel-vram-booster build and install tasks.
#
# `build` needs a Rust toolchain; `install` only copies what is already in
# daemon/target/release, so the two can run on different machines.
#
#   just build
#   just install

set shell := ["bash", "-euo", "pipefail", "-c"]

binary      := "umbriel-vram-booster"
ctl         := "umbriel-vram-boosterctl"
bin_src     := "daemon/target/release/" + binary
ctl_src     := "daemon/target/release/" + ctl
bin_dest    := "/usr/bin/" + binary
ctl_dest    := "/usr/bin/" + ctl
service     := "packaging/usr/lib/systemd/user/" + binary + ".service"
service_dir := "/usr/lib/systemd/user"

default:
    @just --list

# Release build of the daemon and the ctl.
build:
    cd daemon && cargo build --release

# Lints: rustfmt and clippy -D warnings.
check:
    cd daemon && cargo fmt --check
    cd daemon && cargo clippy --all-targets -- -D warnings

# Unit tests.
test:
    cd daemon && cargo test

# Binaries come from `just build` (or any other checkout/toolchain); install
# never builds, so a host without a Rust toolchain can still install.
[private]
check-bins:
    @test -x {{bin_src}} && test -x {{ctl_src}} || \
        { echo "error: {{bin_src}} or {{ctl_src}} missing; run 'just build' first (needs a Rust toolchain)" >&2; exit 1; }

# Check the kernel, dmemcg-booster and Umbriel this machine runs.
check-deps:
    @grep -qw dmem /sys/fs/cgroup/cgroup.controllers || \
        { echo "error: 'dmem' controller missing from /sys/fs/cgroup/cgroup.controllers; needs kernel 6.12+ with dmem cgroup support" >&2; exit 1; }
    @systemctl is-active --quiet dmemcg-booster.service || \
        { echo "error: system dmemcg-booster.service is not active; run: sudo systemctl enable --now dmemcg-booster.service" >&2; exit 1; }
    @systemctl --user is-active --quiet dmemcg-booster.service || \
        { echo "error: user dmemcg-booster.service is not active; run: systemctl --user enable --now dmemcg-booster.service" >&2; exit 1; }
    @command -v umbriel >/dev/null || { echo "error: 'umbriel' not on PATH" >&2; exit 1; }
    @echo "deps OK: dmem controller present, system + user dmemcg-booster active, umbriel found"

# Install the release build and enable the user service. Does not build: run `just build` first.
install: check-deps check-bins
    sudo install -Dm755 {{bin_src}} {{bin_dest}}
    sudo install -Dm755 {{ctl_src}} {{ctl_dest}}
    sudo install -Dm644 {{service}} {{service_dir}}/{{binary}}.service
    systemctl --user daemon-reload
    systemctl --user enable --now {{binary}}.service
    @echo ""
    @echo "Installed. The daemon follows the Umbriel socket on its own."

# Disable the user service and remove what install put in place.
uninstall:
    -systemctl --user disable --now {{binary}}.service
    -sudo rm -f {{bin_dest}} {{ctl_dest}} {{service_dir}}/{{binary}}.service
    -systemctl --user daemon-reload
    @echo ""
    @echo "Uninstalled."

# Reinstall built binaries and restart the daemon.
reload: check-bins
    sudo install -Dm755 {{bin_src}} {{bin_dest}}
    sudo install -Dm755 {{ctl_src}} {{ctl_dest}}
    systemctl --user restart {{binary}}.service
    @echo "Daemon restarted."

# Follow the daemon's journal.
logs:
    journalctl --user -u {{binary}}.service -f --no-pager

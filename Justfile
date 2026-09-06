binary      := "umbriel-vram-booster"
ctl         := "umbriel-vram-boosterctl"
bin_src     := "daemon/target/release/" + binary
ctl_src     := "daemon/target/release/" + ctl
bin_dest    := "/usr/bin/" + binary
ctl_dest    := "/usr/bin/" + ctl
service     := "packaging/usr/lib/systemd/user/" + binary + ".service"
service_dir := "/usr/lib/systemd/user"

default: build

build:
    cd daemon && cargo build --release

check:
    cd daemon && cargo fmt --check
    cd daemon && cargo clippy -- -D warnings
    cd daemon && cargo test

# Binaries come from `just build` (or any other checkout/toolchain); install
# never builds, so a host without a Rust toolchain can still install.
check-bins:
    @test -x {{bin_src}} && test -x {{ctl_src}} || \
        { echo "ERROR: {{bin_src}} or {{ctl_src}} missing. Run 'just build' first (needs a Rust toolchain)."; exit 1; }

check-deps:
    @grep -qw dmem /sys/fs/cgroup/cgroup.controllers || \
        { echo "ERROR: 'dmem' controller missing from /sys/fs/cgroup/cgroup.controllers. Need kernel 6.12+ with dmem cgroup support."; exit 1; }
    @systemctl is-active --quiet dmemcg-booster.service || \
        { echo "ERROR: system dmemcg-booster.service is not active. Run: sudo systemctl enable --now dmemcg-booster.service"; exit 1; }
    @systemctl --user is-active --quiet dmemcg-booster.service || \
        { echo "ERROR: user dmemcg-booster.service is not active. Run: systemctl --user enable --now dmemcg-booster.service"; exit 1; }
    @command -v umbriel >/dev/null || { echo "ERROR: 'umbriel' not on PATH."; exit 1; }
    @echo "deps OK: dmem controller present, system + user dmemcg-booster active, umbriel found"

install: check-deps check-bins
    sudo install -Dm755 {{bin_src}} {{bin_dest}}
    sudo install -Dm755 {{ctl_src}} {{ctl_dest}}
    sudo install -Dm644 {{service}} {{service_dir}}/{{binary}}.service
    systemctl --user daemon-reload
    systemctl --user enable --now {{binary}}.service
    @echo ""
    @echo "Installed. The daemon follows the Umbriel socket on its own."

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

logs:
    journalctl --user -u {{binary}}.service -f --no-pager

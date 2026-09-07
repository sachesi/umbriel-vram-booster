# The release profile already strips symbols, so there is nothing to extract.
%global debug_package %{nil}

Name:           umbriel-vram-booster
Version:        0.2.1
Release:        1%{?dist}
Summary:        VRAM priority for the focused window on the Umbriel compositor

License:        GPL-3.0-or-later
URL:            https://github.com/sachesi/umbriel-vram-booster
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
# Crate dependencies, so the build needs no network:
#   tar xf %%{name}-%%{version}.tar.gz && cd %%{name}-%%{version}/daemon
#   cargo vendor ../vendor
#   tar czf %%{name}-%%{version}-vendor.tar.gz -C .. vendor
Source1:        %{name}-%{version}-vendor.tar.gz

ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo >= 1.88
BuildRequires:  rust >= 1.88
BuildRequires:  systemd-rpm-macros

%description
A systemd --user daemon that follows Umbriel's IPC socket, takes the active
window from each snapshot and gives its app.slice unit the bulk of VRAM
through the dmem cgroup controller, reverting the window boosted before it.
umbriel-vram-boosterctl reports what it is doing.

Needs a kernel with the dmem controller (6.12+) and a running dmemcg-booster
to propagate that controller into the user session. Without them the daemon
exits at startup and says which piece is missing.

%prep
%autosetup -n %{name}-%{version} -a 1
mkdir -p .cargo
cat > .cargo/config.toml <<CARGO_EOF
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "$PWD/vendor"
CARGO_EOF

%build
cd daemon
cargo build --release --offline --locked

%check
cd daemon
cargo test --release --offline --locked

%install
install -Dpm0755 daemon/target/release/%{name} %{buildroot}%{_bindir}/%{name}
install -Dpm0755 daemon/target/release/%{name}ctl %{buildroot}%{_bindir}/%{name}ctl
install -Dpm0644 packaging/usr/lib/systemd/user/%{name}.service \
    %{buildroot}%{_userunitdir}/%{name}.service

%post
%systemd_user_post %{name}.service

%preun
%systemd_user_preun %{name}.service

%postun
%systemd_user_postun_with_restart %{name}.service

%files
%license LICENSE
%doc README.md docs/install.md docs/usage.md
%{_bindir}/%{name}
%{_bindir}/%{name}ctl
%{_userunitdir}/%{name}.service

%changelog
* Mon Sep 07 2026 sachesi <xsachesi@pm.me> - 0.2.1-1
- Drop ProtectHome from the unit; it hid the session bus

* Mon Sep 07 2026 sachesi <xsachesi@pm.me> - 0.2.0-1
- Initial package

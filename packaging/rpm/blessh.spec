# blessh for Fedora COPR. The Rust crates are bundled (Source1, made by
# packaging/vendor-tarball.sh): COPR builds offline, and Fedora does not
# package bluer. An official Fedora package would unbundle them instead.

Name:           blessh
Version:        0.3.1
Release:        1%{?dist}
Summary:        SSH over Bluetooth Low Energy: client and server

# blessh is BSD-2-Clause. The bundled crates are MIT OR Apache-2.0 (most),
# MIT, Unlicense OR MIT, and Unicode-3.0; libdbus-sys statically links its
# bundled copy of libdbus, which is AFL-2.1 OR GPL-2.0-or-later.
License:        BSD-2-Clause AND (MIT OR Apache-2.0) AND MIT AND (Unlicense OR MIT) AND Unicode-3.0 AND (AFL-2.1 OR GPL-2.0-or-later)
URL:            https://github.com/tyetladd/blessh
Source0:        blessh-%{version}.tar.gz
Source1:        blessh-%{version}-vendor.tar.xz

ExclusiveArch:  x86_64 aarch64
BuildRequires:  cargo >= 1.89
BuildRequires:  rust >= 1.89
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  systemd-rpm-macros
Requires:       bluez >= 5.55
Requires:       openssh-clients
Requires:       systemd
Recommends:     openssh-server

%description
blessh runs SSH over Bluetooth Low Energy, without pairing: the SSH host key
and login provide the security. As a client, blessh lists the servers in
range and runs the system ssh through the one chosen. As a server, it is off
until "blessh server enable": an unprivileged daemon then registers a GATT
service with BlueZ, listens on L2CAP connection-oriented channels and hands
each session to a per-session "sshd -i" started by systemd socket activation,
with the system SSH server's configuration.

%prep
%autosetup -n blessh-%{version}
tar xJf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'CFG'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CFG

%build
export CARGO_HOME=$PWD/.cargo-home
cargo build --release --offline --locked -p blessh-client

%check
export CARGO_HOME=$PWD/.cargo-home
cargo test --release --offline --locked -p blessh-core

%install
install -Dpm 0755 target/release/blessh %{buildroot}%{_bindir}/blessh
install -Dpm 0755 packaging/linux/eatt-off %{buildroot}%{_prefix}/lib/blessh/eatt-off
install -Dpm 0644 packaging/linux/sshd_config %{buildroot}%{_sysconfdir}/blessh/sshd_config
install -Dpm 0644 packaging/linux/default %{buildroot}%{_sysconfdir}/default/blessh
for u in blessh-server.service blessh-session.socket blessh-session@.service; do
    install -Dpm 0644 packaging/linux/systemd/$u %{buildroot}%{_unitdir}/$u
done
# Fedora's BlueZ has no "bluetooth" group (Debian's creates it), and
# blessh-server.service runs with SupplementaryGroups=bluetooth: create it.
install -d %{buildroot}%{_sysusersdir}
{ echo 'g bluetooth -'; cat packaging/linux/sysusers.conf; } > %{buildroot}%{_sysusersdir}/blessh.conf
chmod 0644 %{buildroot}%{_sysusersdir}/blessh.conf
install -Dpm 0644 packaging/linux/dbus.conf %{buildroot}%{_datadir}/dbus-1/system.d/blessh.conf

%post
systemd-sysusers %{_sysusersdir}/blessh.conf >/dev/null 2>&1 || :
%systemd_post blessh-server.service blessh-session.socket

%preun
%systemd_preun blessh-server.service blessh-session.socket

%postun
%systemd_postun_with_restart blessh-server.service blessh-session.socket
if [ "$1" -eq 0 ] && [ -e /run/blessh-ecred-off ]; then
    echo Y > /sys/module/bluetooth/parameters/enable_ecred 2>/dev/null || :
    rm -f /run/blessh-ecred-off
fi

%files
%license LICENSE
%doc README.md docs/protocol.md
%{_bindir}/blessh
%dir %{_prefix}/lib/blessh
%{_prefix}/lib/blessh/eatt-off
%dir %{_sysconfdir}/blessh
%config(noreplace) %{_sysconfdir}/blessh/sshd_config
%config(noreplace) %{_sysconfdir}/default/blessh
%{_unitdir}/blessh-server.service
%{_unitdir}/blessh-session.socket
%{_unitdir}/blessh-session@.service
%{_sysusersdir}/blessh.conf
%{_datadir}/dbus-1/system.d/blessh.conf

%changelog
* Thu Nov 26 2026 blessh packagers - 0.3.1-1
- First COPR build: resumable sessions on L2CAP channels (--resume)

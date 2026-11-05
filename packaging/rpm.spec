Name: cyime-wayclip
Version: 0.2.0
Release: 1%{?dist}
Summary: Wayland clipboard synchronization for CyIME
License: GPL-3.0-only
BuildRequires: cargo
Source0: %{name}-%{version}.tar.gz

%description
Secure clipboard synchronization between a Wayland desktop and CyIME.

%prep
%setup -q

%build
cargo build --release --all-features

%install
install -Dpm0755 target/release/cyime-wayclip %{buildroot}%{_bindir}/cyime-wayclip
install -Dpm0644 packaging/cyime-wayclip.service %{buildroot}%{_userunitdir}/cyime-wayclip.service
sed -i 's#@bindir@#%{_bindir}#' %{buildroot}%{_userunitdir}/cyime-wayclip.service

%files
%license LICENSE
%doc README.md
%{_bindir}/cyime-wayclip
%{_userunitdir}/cyime-wayclip.service

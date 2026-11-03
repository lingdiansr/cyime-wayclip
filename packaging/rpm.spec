Name: cyime-wayclip
Version: 0.1.0
Release: 1%{?dist}
Summary: Wayland clipboard synchronization for CyIME
License: MIT
BuildRequires: cargo

%description
Secure clipboard synchronization between a Wayland desktop and CyIME.

%prep
%setup -q -T -c
cp -a %{_sourcedir}/* .

%build
cargo build --release --all-features

%install
install -Dpm0755 target/release/cyime-wayclip %{buildroot}%{_bindir}/cyime-wayclip
install -Dpm0644 packaging/cyime-wayclip.service %{buildroot}%{_userunitdir}/cyime-wayclip.service
sed -i 's#@bindir@#%{_bindir}#' %{buildroot}%{_userunitdir}/cyime-wayclip.service

%files
%{_bindir}/cyime-wayclip
%{_userunitdir}/cyime-wayclip.service

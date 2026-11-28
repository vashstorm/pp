%global debug_package %{nil}

Name:           paper-plane-pp
Version:        %{pp_version}
Release:        %{pp_release}%{?dist}
Summary:        Terminal coding agent
License:        %{pp_license}
URL:            https://github.com/vashstorm/pp
Source0:        pp
Requires:       bash, ca-certificates
Recommends:     bubblewrap >= 0.5
Recommends:     libnotify

%description
pp runs interactive and one-shot coding tasks in the terminal.

%install
install -D -m 0755 "%{SOURCE0}" "%{buildroot}%{_bindir}/pp"

%files
%{_bindir}/pp

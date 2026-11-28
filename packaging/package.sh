#!/bin/sh
set -eu

fail() {
    printf 'pp packaging: %s\n' "$*" >&2
    exit 1
}

require() {
    for tool do
        command -v "$tool" >/dev/null 2>&1 || fail "Required command is missing: $tool"
    done
}

build_macos() {
    mkdir -p "$stage/image"
    install -m 0755 "$binary" "$stage/image/pp"
    install -m 0755 "$repo/packaging/install-local.sh" "$stage/image/Install pp.command"

    if [ -n "${PP_MACOS_APP_IDENTITY:-}" ]; then
        codesign --force --options runtime --timestamp --identifier io.github.vashstorm.pp \
            --sign "$PP_MACOS_APP_IDENTITY" "$stage/image/pp"
    fi
    if [ "$macos_image_tool" = diskutil ]; then
        diskutil image create from --format UDZO --volumeName "pp ($profile)" \
            "$stage/image" "$artifact"
    else
        hdiutil create -format UDZO -fs HFS+ -volname "pp ($profile)" \
            -srcfolder "$stage/image" "$artifact"
    fi

    if [ -n "${PP_MACOS_APP_IDENTITY:-}" ]; then
        codesign --timestamp --identifier io.github.vashstorm.pp.dmg \
            --sign "$PP_MACOS_APP_IDENTITY" "$artifact"
    fi
    if [ -n "${PP_MACOS_NOTARY_PROFILE:-}" ]; then
        xcrun notarytool submit "$artifact" --keychain-profile "$PP_MACOS_NOTARY_PROFILE" --wait
        xcrun stapler staple "$artifact"
        xcrun stapler validate "$artifact"
    fi
}

build_deb() {
    deb_root="debian/$package"
    mkdir -p "$stage/$deb_root/DEBIAN" "$stage/$deb_root/usr/bin"
    install -m 0755 "$binary" "$stage/$deb_root/usr/bin/pp"
    cat >"$stage/debian/control" <<EOF
Source: $package
Maintainer: pp maintainers

Package: $package
Architecture: any
Description: Terminal coding agent
EOF
    dependencies=$(cd "$stage" && dpkg-shlibdeps -O -e "$deb_root/usr/bin/pp")
    case "$dependencies" in
        shlibs:Depends=*) dependencies=${dependencies#shlibs:Depends=} ;;
        *) fail 'Could not determine shared library dependencies for the DEB.' ;;
    esac

    deb_version=$version
    if [ "$profile" = dev ]; then
        deb_version="$deb_version~dev"
    fi
    cat >"$stage/$deb_root/DEBIAN/control" <<EOF
Package: $package
Version: $deb_version
Architecture: $package_arch
Maintainer: pp maintainers
Section: devel
Priority: optional
Depends: bash, ca-certificates, $dependencies
Recommends: bubblewrap (>= 0.5), libnotify-bin
Homepage: https://github.com/vashstorm/pp
Description: Terminal coding agent
 pp runs interactive and one-shot coding tasks in the terminal.
EOF
    dpkg-deb --root-owner-group --build "$stage/$deb_root" "$artifact"
}

build_rpm() {
    rpm_release=1
    if [ "$profile" = dev ]; then
        rpm_release=0.dev
    fi
    mkdir -p "$stage/rpm/SOURCES"
    install -m 0755 "$binary" "$stage/rpm/SOURCES/pp"
    rpmbuild -bb --define "_topdir $stage/rpm" \
        --define "pp_version $rpm_version" --define "pp_release $rpm_release" \
        --define "pp_license ${PP_RPM_LICENSE:-LicenseRef-pp-Unspecified}" \
        "$repo/packaging/pp.spec"
    set -- "$stage/rpm/RPMS/$package_arch/"*.rpm
    [ "$#" -eq 1 ] && [ -f "$1" ] || fail 'Expected exactly one binary RPM.'
    mv "$1" "$artifact"
    if [ -n "${PP_RPM_SIGN_KEY:-}" ]; then
        rpmsign --addsign --key-id "$PP_RPM_SIGN_KEY" "$artifact"
    fi
}

[ "$#" -eq 4 ] || fail 'Invoke packaging through the Makefile.'
action=$1
profile=$2
platform=$3
arch=$4
case "$action" in build|install) ;; *) fail "Unsupported action: $action" ;; esac
case "$profile" in
    release) cargo_dir=release ;;
    dev) cargo_dir=debug ;;
    *) fail "Unsupported build profile: $profile" ;;
esac

package=paper-plane-pp
repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$repo"
case "$arch" in aarch64|x86_64) ;; *) fail "Unsupported host architecture: $arch" ;; esac

case "$platform:$action" in
    macos:install)
        expected_host="$arch-apple-darwin"
        ;;
    macos:build)
        expected_host="$arch-apple-darwin"
        package_arch=$arch
        if [ "$arch" = aarch64 ]; then package_arch=arm64; fi
        extension=dmg
        macos_version=$(sw_vers -productVersion)
        # 从 macOS 27 起，diskutil 支持直接从目录创建映像。
        if [ "${macos_version%%.*}" -ge 27 ]; then
            macos_image_tool=diskutil
        else
            macos_image_tool=hdiutil
        fi
        require "$macos_image_tool"
        if [ -n "${PP_MACOS_APP_IDENTITY:-}" ]; then require codesign; fi
        if [ -n "${PP_MACOS_NOTARY_PROFILE:-}" ]; then
            [ -n "${PP_MACOS_APP_IDENTITY:-}" ] || fail 'Notarization requires PP_MACOS_APP_IDENTITY.'
            require xcrun
        fi
        ;;
    linux:install)
        expected_host="$arch-unknown-linux-gnu"
        ;;
    fedora:build)
        expected_host="$arch-unknown-linux-gnu"
        package_arch=$arch
        extension=rpm
        require rpmbuild
        if [ -n "${PP_RPM_SIGN_KEY:-}" ]; then require rpmsign; fi
        ;;
    ubuntu:build)
        expected_host="$arch-unknown-linux-gnu"
        case "$arch" in aarch64) package_arch=arm64 ;; x86_64) package_arch=amd64 ;; esac
        extension=deb
        require dpkg-deb dpkg-shlibdeps
        ;;
    *) fail "Unsupported action '$action' for platform '$platform'." ;;
esac

compiler=${RUSTC:-rustc}
require cargo "$compiler"
rust_host=$("$compiler" --print host-tuple)
[ "$rust_host" = "$expected_host" ] || fail "Native Rust toolchain required for $expected_host (found $rust_host)."

target_dir="$repo/target"
if [ "$action:$profile" = install:dev ]; then
    require jq
    # 沿用 cargo build 的构建配置，并从 Cargo 的产物消息获取实际路径。
    messages=$(cargo build --locked -p pp --bin pp --message-format=json-render-diagnostics)
    binary=$(printf '%s\n' "$messages" | jq -ers '
        [.[] | select(.reason == "compiler-artifact" and .target.name == "pp"
                       and .target.kind == ["bin"]) | .executable // empty]
        | if length == 1 then .[0] else empty end
    ') || fail 'Expected exactly one pp executable from Cargo.'
else
    # 发行构建显式使用已校验的本机目标，并静态链接 PCRE2。
    set -- --locked -p pp --bin pp --target "$rust_host" --target-dir "$target_dir"
    if [ "$profile" = release ]; then set -- "$@" --release; fi
    RUSTC="$compiler" PCRE2_SYS_STATIC=1 cargo build "$@"
    binary="$target_dir/$rust_host/$cargo_dir/pp"
fi

if [ "$action" = install ]; then
    "$binary" init-config
    exec sh "$repo/packaging/install-local.sh" "$binary"
fi

version=$("$binary" --version)
case "$version" in 'pp '*) version=${version#pp } ;; *) fail 'Could not determine the version of pp.' ;; esac
case "$version" in ''|*[!0-9A-Za-z.+-]*|[!0-9]*) fail "Invalid package version: $version" ;; esac
rpm_version=$(printf '%s\n' "${version%%+*}" | sed 's/-/~/g')

output="$target_dir/packages"
mkdir -p "$output"
stage=$(mktemp -d "$output/.stage.XXXXXXXX")
trap 'exit_code=$?; rm -rf "$stage" || exit_code=1; exit "$exit_code"' EXIT
trap 'exit 1' HUP INT TERM
filename="pp-$version-$profile-$package_arch.$extension"
artifact="$stage/$filename"
case "$platform" in
    macos) build_macos ;;
    ubuntu) build_deb ;;
    fedora) build_rpm ;;
esac
"$binary" init-config

mv -f "$artifact" "$output/$filename"
printf '%s\n' "$output/$filename"

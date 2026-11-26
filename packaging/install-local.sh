#!/bin/sh
set -eu

case "${HOME:-}" in
    /*) ;;
    *) printf 'An absolute HOME is required to install pp.\n' >&2; exit 1 ;;
esac

binary=${1:-}
if [ -z "$binary" ]; then
    image_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
    binary="$image_dir/pp"
fi
bin_dir="$HOME/.local/bin"
destination="$bin_dir/pp"
mkdir -p "$bin_dir"
if [ -d "$destination" ]; then
    printf 'Cannot install pp at %s because it is a directory.\n' "$destination" >&2
    exit 1
fi

# 在目标目录暂存新文件，复制成功后原子替换现有二进制。
temporary=$(mktemp "$bin_dir/.pp.XXXXXXXX")
trap 'rm -f "$temporary"' EXIT
trap 'exit 1' HUP INT TERM
install -m 0755 "$binary" "$temporary"
mv -f "$temporary" "$destination"
printf 'Installed %s\n' "$destination"
"$destination" --version

selected=$(command -v pp || true)
if ! [ "$selected" -ef "$destination" ]; then
    printf 'Add %s to the front of your PATH to run this copy of pp.\n' "$bin_dir" >&2
fi

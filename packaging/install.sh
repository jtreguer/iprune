#!/bin/sh
# Installs iprune and iprune-gui with cargo, autotag next to them (iprune's
# import runs it, and looks for it there), and the desktop entry that puts
# the window in the app launcher. Run from anywhere; `--uninstall` removes
# all four.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
# The entry needs an absolute path; cargo reads a relative one from here.
case $root in /*) ;; *) root=$PWD/$root ;; esac
apps=${XDG_DATA_HOME:-$HOME/.local/share}/applications
entry=$apps/iprune.desktop

if [ "${1-}" = --uninstall ]; then
    cargo uninstall --root "$root" iprune iprune-gui || true
    rm -f "$root/bin/autotag" "$entry"
    exit 0
fi

cargo install --locked --root "$root" --path "$repo/crates/iprune"
cargo install --locked --root "$root" --path "$repo/crates/iprune-gui"
# A copy, through a temporary file and a rename: an autotag running keeps
# its file. ~/Music/autotag, a link to the repository's, is left alone.
mkdir -p "$root/bin"
cp "$repo/tools/autotag" "$root/bin/.autotag.tmp"
chmod 755 "$root/bin/.autotag.tmp"
mv "$root/bin/.autotag.tmp" "$root/bin/autotag"
if ! command -v uv >/dev/null 2>&1; then
    echo "note: autotag runs with uv, which is not on the PATH (pacman -S uv)" >&2
fi
# The launcher may not have cargo's bin folder on its PATH: the entry names
# the installed binary in full, quoted as the desktop entry spec says (\ " `
# and $ escaped inside the quotes, then every backslash doubled, % doubled).
exec=$(printf '%s' "$root/bin/iprune-gui" |
    sed -e 's/[\\"`$]/\\&/g' -e 's/\\/\\\\/g' -e 's/%/%%/g')
mkdir -p "$apps"
while IFS= read -r line; do
    case $line in
    Exec=*) printf 'Exec="%s"\n' "$exec" ;;
    *) printf '%s\n' "$line" ;;
    esac
done < "$repo/packaging/iprune.desktop" > "$entry.tmp"
mv "$entry.tmp" "$entry"
echo "installed $root/bin/iprune, $root/bin/iprune-gui, $root/bin/autotag and $entry"

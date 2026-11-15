#!/bin/sh
# Installs iprune and iprune-gui with cargo, autotag next to them (iprune's
# import runs it, and looks for it there), and the desktop entry that puts
# the window in the app launcher. Then, through sudo, the clock helper that
# sets the iPod clock at each sync, root-owned in /usr/local/lib/iprune, and
# the polkit policy that lets the desktop session run it without a
# password. Run from anywhere; `--uninstall` removes all six.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
# The entry needs an absolute path; cargo reads a relative one from here.
case $root in /*) ;; *) root=$PWD/$root ;; esac
apps=${XDG_DATA_HOME:-$HOME/.local/share}/applications
entry=$apps/iprune.desktop
# Both paths are fixed: the policy names the helper, and polkit reads
# policies only from /usr/share.
helper_dir=/usr/local/lib/iprune
policy=/usr/share/polkit-1/actions/dev.iprune.clock.policy

if [ "${1-}" = --uninstall ]; then
    cargo uninstall --root "$root" iprune iprune-gui || true
    rm -f "$root/bin/autotag" "$entry"
    echo "removing the clock helper and its polkit policy (sudo)"
    if sudo rm -f "$helper_dir/iprune-clock" "$policy"; then
        sudo rmdir "$helper_dir" 2>/dev/null || true
    else
        echo "note: the clock helper and its policy were not removed" >&2
    fi
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

# Only root may set the iPod clock. The helper is built as this user, then
# copied root-owned where no other user can change it, since the policy
# lets the session run it as root without a password.
build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT
cargo install --locked --quiet --root "$build" --path "$repo/crates/iprune-clock"
echo "installing the clock helper in $helper_dir and its polkit policy (sudo)"
# The folder first, so one that already exists is root's too: whoever can
# write in it could swap the helper.
if sudo install -d -o root -g root -m 755 "$helper_dir" &&
    sudo install -o root -g root -m 755 "$build/bin/iprune-clock" "$helper_dir/iprune-clock" &&
    sudo install -D -o root -g root -m 644 "$repo/packaging/dev.iprune.clock.policy" "$policy"; then
    echo "installed $helper_dir/iprune-clock and $policy"
else
    echo "note: the clock helper was not installed; syncs will warn that the iPod clock was not set" >&2
fi
if [ ! -x /usr/bin/sg_raw ]; then
    echo "note: the clock helper needs sg_raw (pacman -S sg3_utils)" >&2
fi

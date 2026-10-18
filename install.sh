#!/bin/sh
# Installs the udev rule aula needs to reach the keyboard, puts the asset tree
# where the binary can find it, and drops a launcher on PATH. Run it from the
# directory this script lives in, or by full path.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
rule="$here/udev/60-aula.rules"

if [ ! -f "$rule" ]; then
    echo "aula: $rule is missing; is the archive complete?" >&2
    exit 1
fi

# A release tarball unpacks `aula` next to `assets/`; a source checkout builds
# into target/release and keeps the assets at the top of the tree.
bin="$here/aula"
if [ ! -x "$bin" ]; then
    bin="$here/target/release/aula"
fi
if [ ! -x "$bin" ]; then
    echo "aula: no aula binary in $here; run 'cargo build --release' first" >&2
    exit 1
fi

assets="$here/assets"
if [ ! -d "$assets/skins" ]; then
    echo "aula: $assets/skins is missing; the window and the he-* commands" >&2
    echo "      will fail with \"cannot find the assets directory\"" >&2
fi

echo "aula is installed in $here"
echo
echo "Installing the udev rule needs root, because /dev/hidraw* is root-only"
echo "until a rule grants the logged-in user access."
# An earlier version installed this at 99-, which is too late for systemd to
# grant the uaccess ACL. Clear it out so the two do not both sit there.
sudo rm -f /etc/udev/rules.d/99-aula.rules
sudo cp "$rule" /etc/udev/rules.d/60-aula.rules
sudo udevadm control --reload
sudo udevadm trigger

# The binary looks for its assets next to itself, in ./assets, and finally in
# /usr/share/aula/assets and /usr/local/share/aula/assets. A tarball install
# already has them beside the binary, but a source checkout does not, so put a
# copy where the search finds it no matter where aula is started from.
if [ "$(dirname "$bin")" != "$here" ] && [ -d "$assets/skins" ]; then
    echo
    echo "Installing the asset tree in /usr/local/share/aula/assets"
    sudo mkdir -p /usr/local/share/aula
    sudo rm -rf /usr/local/share/aula/assets
    sudo cp -R "$assets" /usr/local/share/aula/assets
fi

mkdir -p "$HOME/.local/bin"
ln -sf "$bin" "$HOME/.local/bin/aula"
echo
echo "Done. Unplug and replug the keyboard, then run:  aula"
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "(add $HOME/.local/bin to your PATH, or run $bin directly)" ;;
esac

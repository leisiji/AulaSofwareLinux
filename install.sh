#!/bin/sh
# Installs the udev rule aula needs to reach the keyboard, and drops a launcher
# on PATH. Run it from the directory this script lives in, or by full path.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
rule="$here/udev/99-aula.rules"

if [ ! -f "$rule" ]; then
    echo "aula: $rule is missing; is the archive complete?" >&2
    exit 1
fi

echo "aula is installed in $here"
echo
echo "Installing the udev rule needs root, because /dev/hidraw* is root-only"
echo "until a rule grants the logged-in user access."
sudo cp "$rule" /etc/udev/rules.d/99-aula.rules
sudo udevadm control --reload
sudo udevadm trigger

mkdir -p "$HOME/.local/bin"
ln -sf "$here/aula" "$HOME/.local/bin/aula"
echo
echo "Done. Unplug and replug the keyboard, then run:  aula"
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "(add $HOME/.local/bin to your PATH, or run $here/aula directly)" ;;
esac

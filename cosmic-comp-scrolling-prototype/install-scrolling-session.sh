#!/bin/sh

set -eu

SCRIPT_PATH=$(readlink -f -- "$0")
SUITE_ROOT=$(CDPATH= cd -- "$(dirname -- "$SCRIPT_PATH")/.." && pwd -P)

echo "This compatibility command now installs both the compositor and applet."
exec "$SUITE_ROOT/install.sh" "$@"

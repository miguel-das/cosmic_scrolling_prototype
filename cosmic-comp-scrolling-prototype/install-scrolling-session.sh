#!/bin/sh

set -eu

SCRIPT_PATH=$(readlink -f -- "$0")
PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd -P)
DESTDIR=${DESTDIR-}

usage() {
    echo "Usage: $0 [--destdir ABSOLUTE_DIRECTORY]"
    echo "Install this clone's COSMIC Scrolling Test login session."
    echo "DESTDIR (or --destdir) stages the files without administrator access."
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --destdir)
            [ "$#" -ge 2 ] || { usage >&2; exit 2; }
            DESTDIR=$2
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

case "$DESTDIR" in
    ""|/*) ;;
    *) echo "DESTDIR must be an absolute directory." >&2; exit 2 ;;
esac

if [ ! -x "$PROJECT_ROOT/target/debug/cosmic-comp" ]; then
    echo "Build the compositor first: cd \"$PROJECT_ROOT\" && cargo build --locked" >&2
    exit 1
fi
if [ ! -x "$PROJECT_ROOT/start-scrolling-session.sh" ]; then
    echo "Session launcher is missing or not executable." >&2
    exit 1
fi
if [ ! -f "$PROJECT_ROOT/cosmic-scrolling-test.desktop" ]; then
    echo "Session desktop entry is missing: $PROJECT_ROOT/cosmic-scrolling-test.desktop" >&2
    exit 1
fi
if [ -z "$DESTDIR" ] && [ ! -x /usr/bin/start-cosmic ]; then
    echo "Install the system COSMIC desktop first (/usr/bin/start-cosmic is missing)." >&2
    exit 1
fi

LAUNCHER="$DESTDIR/usr/local/bin/cosmic-scrolling-test-session"
DESKTOP="$DESTDIR/usr/share/wayland-sessions/cosmic-scrolling-test.desktop"
OWNER='X-CosmicScrollingOwner=cosmic-scrolling-prototype-v1'
OWNED_DESKTOP=false
if [ -e "$DESKTOP" ] || [ -L "$DESKTOP" ]; then
    if [ -L "$DESKTOP" ] || [ ! -f "$DESKTOP" ] || ! grep -qxF "$OWNER" "$DESKTOP"; then
        echo "Refusing to replace an unrecognized session entry: $DESKTOP" >&2
        exit 1
    fi
    OWNED_DESKTOP=true
fi
if [ -e "$LAUNCHER" ] || [ -L "$LAUNCHER" ]; then
    if [ ! -L "$LAUNCHER" ]; then
        echo "Refusing to replace a non-symlink launcher: $LAUNCHER" >&2
        exit 1
    fi
    EXISTING_TARGET=$(readlink -- "$LAUNCHER")
    if [ "$EXISTING_TARGET" != "$PROJECT_ROOT/start-scrolling-session.sh" ]; then
        if [ "$OWNED_DESKTOP" != true ] || [ "$(basename -- "$EXISTING_TARGET")" != start-scrolling-session.sh ]; then
            echo "Refusing to replace an unrecognized launcher: $LAUNCHER" >&2
            exit 1
        fi
    fi
fi

# Elevate only for the real greeter installation, never for a staging directory.
if [ -z "$DESTDIR" ] && [ "$(id -u)" -ne 0 ]; then
    exec sudo -- "$SCRIPT_PATH" --destdir ""
fi

install -d -- "$(dirname -- "$LAUNCHER")" "$(dirname -- "$DESKTOP")"
ln -sfnT -- "$PROJECT_ROOT/start-scrolling-session.sh" "$LAUNCHER"
install -m 0644 -- "$PROJECT_ROOT/cosmic-scrolling-test.desktop" "$DESKTOP"
if [ -n "$DESTDIR" ]; then
    echo "Session files staged under $DESTDIR (the live greeter was not changed)."
else
    echo "Installed COSMIC Scrolling Test for $PROJECT_ROOT."
    echo "Save your work, log out, and select COSMIC Scrolling Test in the greeter."
fi

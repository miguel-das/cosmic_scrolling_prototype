#!/bin/sh

set -eu

OWNER_ID=cosmic-scrolling-prototype-v1
SYSTEM_LAUNCHER=/usr/local/bin/cosmic-scrolling-test-session
SYSTEM_DESKTOP=/usr/share/wayland-sessions/cosmic-scrolling-test.desktop

die() {
    printf 'uninstall.sh: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '%s\n' "$*"
}

run_as_root() {
    if [ -n "$DESTDIR" ] || [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        command -v sudo >/dev/null 2>&1 || die "required command not found: sudo"
        sudo "$@"
    fi
}

PURGE_CONFIG=false
DESTDIR=${DESTDIR-}
while [ "$#" -gt 0 ]; do
case "${1:-}" in
    -h|--help)
        cat <<'EOF'
Usage: ./uninstall.sh [--purge-config] [--destdir ABSOLUTE_DIRECTORY]
Use --destdir (or DESTDIR) to remove staged greeter files without sudo.
The project-private applet is removed in either mode.

Remove the COSMIC Scrolling Test greeter entry and private applet installation.
Build caches are retained. Isolated session settings are retained unless
--purge-config is supplied.
EOF
        exit 0
        ;;
    --purge-config) PURGE_CONFIG=true; shift ;;
    --destdir)
        [ "$#" -ge 2 ] || die "--destdir requires an absolute directory"
        DESTDIR=$2; shift 2 ;;
    '') die "empty argument" ;;
    *) die "unknown argument: $1 (try --help)" ;;
esac
done
case "$DESTDIR" in
    ""|/*) ;;
    *) die "DESTDIR must be an absolute directory" ;;
esac
SYSTEM_LAUNCHER="$DESTDIR$SYSTEM_LAUNCHER"
SYSTEM_DESKTOP="$DESTDIR$SYSTEM_DESKTOP"

command -v readlink >/dev/null 2>&1 || die "required command not found: readlink"
SCRIPT_PATH=$(readlink -f -- "$0") || die "cannot resolve the uninstaller path"
SUITE_ROOT=$(CDPATH= cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd -P)
COMP_ROOT="$SUITE_ROOT/cosmic-comp-scrolling-prototype"
STATE_ROOT="$SUITE_ROOT/.cosmic-scrolling"
PREFIX="$STATE_ROOT/prefix"
SOURCE_LAUNCHER="$COMP_ROOT/start-scrolling-session.sh"
CONFIG_ROOT="$COMP_ROOT/target/scrolling-test-config"

# Do not follow redirected installation directories when writing/removing files.
for owned_directory in "$STATE_ROOT" "$PREFIX" "$PREFIX/bin" "$PREFIX/share" \
    "$PREFIX/share/applications" "$PREFIX/share/icons" \
    "$PREFIX/share/icons/hicolor" "$PREFIX/share/icons/hicolor/scalable" \
    "$PREFIX/share/icons/hicolor/scalable/apps"; do
    [ ! -L "$owned_directory" ] || die "refusing a symlinked installation directory: $owned_directory"
done
[ ! -L "$STATE_ROOT/manifest" ] || die "refusing a symlinked ownership manifest"

# Check ownership of every shared path before removing anything.
if [ -e "$SYSTEM_DESKTOP" ] || [ -L "$SYSTEM_DESKTOP" ]; then
    [ ! -L "$SYSTEM_DESKTOP" ] && [ -f "$SYSTEM_DESKTOP" ] \
        || die "refusing to remove a non-regular desktop entry: $SYSTEM_DESKTOP"
    grep -q "^X-CosmicScrollingOwner=$OWNER_ID\$" "$SYSTEM_DESKTOP" 2>/dev/null \
        || die "refusing to remove an unowned desktop entry: $SYSTEM_DESKTOP"
fi

if [ -e "$SYSTEM_LAUNCHER" ] || [ -L "$SYSTEM_LAUNCHER" ]; then
    [ -L "$SYSTEM_LAUNCHER" ] \
        || die "refusing to remove a non-symlink launcher: $SYSTEM_LAUNCHER"
    LINK_TARGET=$(readlink "$SYSTEM_LAUNCHER" 2>/dev/null || true)
    [ "$LINK_TARGET" = "$SOURCE_LAUNCHER" ] \
        || die "refusing to remove launcher pointing somewhere else: $SYSTEM_LAUNCHER -> ${LINK_TARGET:-<unreadable>}"
fi

PRIVATE_FILES_EXIST=false
if [ -L "$PREFIX/bin/cosmic-comp" ] \
    || [ -e "$PREFIX/bin/cosmic-comp" ] \
    || [ -L "$PREFIX/bin/cosmic-applet-tiling" ] \
    || [ -e "$PREFIX/bin/cosmic-applet-tiling" ] \
    || [ -e "$PREFIX/share/applications/com.system76.CosmicAppletTiling.desktop" ]; then
    PRIVATE_FILES_EXIST=true
fi
if [ -e "$STATE_ROOT/manifest" ]; then
    grep -q "^owner=$OWNER_ID\$" "$STATE_ROOT/manifest" \
        || die "refusing to remove state owned by another installer: $STATE_ROOT"
elif [ "$PRIVATE_FILES_EXIST" = true ]; then
    die "refusing to remove an unowned private applet prefix: $PREFIX"
fi

if [ "${XDG_CONFIG_HOME:-}" = "$CONFIG_ROOT" ]; then
    die "log into normal COSMIC before uninstalling the active test session"
fi

if [ -e "$SYSTEM_DESKTOP" ]; then
    run_as_root rm -f -- "$SYSTEM_DESKTOP"
    note "Removed $SYSTEM_DESKTOP"
else
    note "Already absent: $SYSTEM_DESKTOP"
fi

if [ -e "$SYSTEM_LAUNCHER" ] || [ -L "$SYSTEM_LAUNCHER" ]; then
    run_as_root rm -f -- "$SYSTEM_LAUNCHER"
    note "Removed $SYSTEM_LAUNCHER"
else
    note "Already absent: $SYSTEM_LAUNCHER"
fi

if [ -e "$STATE_ROOT/manifest" ]; then
    for installed_file in \
        "$PREFIX/bin/cosmic-comp" \
        "$PREFIX/bin/cosmic-applet-tiling" \
        "$PREFIX/share/applications/com.system76.CosmicAppletTiling.desktop" \
        "$PREFIX/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletTiling-symbolic.svg" \
        "$PREFIX/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletTiling.Off.svg" \
        "$PREFIX/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletTiling.On.svg" \
        "$PREFIX/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletTiling.Scrolling.svg" \
        "$STATE_ROOT/manifest"
    do
        rm -f -- "$installed_file"
    done
fi

rmdir -- "$PREFIX/bin" 2>/dev/null || true
rmdir -- "$PREFIX/share/applications" 2>/dev/null || true
rmdir -- "$PREFIX/share/icons/hicolor/scalable/apps" 2>/dev/null || true
rmdir -- "$PREFIX/share/icons/hicolor/scalable" 2>/dev/null || true
rmdir -- "$PREFIX/share/icons/hicolor" 2>/dev/null || true
rmdir -- "$PREFIX/share/icons" 2>/dev/null || true
rmdir -- "$PREFIX/share" 2>/dev/null || true
rmdir -- "$PREFIX" 2>/dev/null || true
note "Removed the project-private applet installation."

if [ "$PURGE_CONFIG" = true ]; then
    case "$CONFIG_ROOT" in
        "$COMP_ROOT"/target/scrolling-test-config) ;;
        *) die "refusing unsafe configuration path: $CONFIG_ROOT" ;;
    esac
    [ "$CONFIG_ROOT" != "$COMP_ROOT" ] || die "refusing to remove the compositor root"
    rm -rf -- "$CONFIG_ROOT"
    note "Removed isolated settings: $CONFIG_ROOT"
else
    note "Retained isolated settings: $CONFIG_ROOT"
    note "Use ./uninstall.sh --purge-config to remove them."
fi

note "Retained build caches and source trees under $SUITE_ROOT."
note "The distribution COSMIC session and applet were not changed."

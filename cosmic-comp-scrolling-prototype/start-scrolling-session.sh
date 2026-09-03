#!/bin/sh

set -eu

SCRIPT_PATH=$(readlink -f -- "$0")
PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd -P)
SUITE_ROOT=$(CDPATH= cd -- "$PROJECT_ROOT/.." && pwd -P)
PRIVATE_PREFIX="$SUITE_ROOT/.cosmic-scrolling/prefix"
COMPOSITOR="$PROJECT_ROOT/target/debug/cosmic-comp"
APPLET="$PRIVATE_PREFIX/bin/cosmic-applet-tiling"
TEST_CONFIG_HOME="$PROJECT_ROOT/target/scrolling-test-config"
TEST_COMP_CONFIG="$TEST_CONFIG_HOME/cosmic/com.system76.CosmicComp/v1"

if [ ! -x "$COMPOSITOR" ]; then
    echo "Scrolling test compositor is missing: $COMPOSITOR" >&2
    echo "Build it with: cd $PROJECT_ROOT && cargo build --locked" >&2
    exit 1
fi

if [ ! -x "$APPLET" ]; then
    echo "Scrolling test applet is missing: $APPLET" >&2
    echo "Build and install it with: $SUITE_ROOT/install.sh" >&2
    exit 1
fi

mkdir -p "$TEST_COMP_CONFIG"

# Keep this isolated development session useful on first launch without
# coupling the scrolling engine to autotiling inside the compositor. Preserve
# any explicit choice made later in the test session.
if [ ! -e "$TEST_COMP_CONFIG/autotile" ]; then
    printf '%s\n' true >"$TEST_COMP_CONFIG/autotile"
fi

if [ ! -e "$TEST_COMP_CONFIG/tiling_engine" ]; then
    printf '%s\n' Scrolling >"$TEST_COMP_CONFIG/tiling_engine"
fi

# start-cosmic imports the launch environment into the persistent user systemd
# manager. Restore the pre-session values on logout so the private paths cannot
# leak into a later normal COSMIC session.
ORIGINAL_PATH=$PATH
ORIGINAL_XDG_DATA_DIRS=${XDG_DATA_DIRS-}
ORIGINAL_XDG_DATA_DIRS_SET=${XDG_DATA_DIRS+x}
ORIGINAL_XDG_CONFIG_HOME=${XDG_CONFIG_HOME-}
ORIGINAL_XDG_CONFIG_HOME_SET=${XDG_CONFIG_HOME+x}
ORIGINAL_RUST_LOG=${RUST_LOG-}
ORIGINAL_RUST_LOG_SET=${RUST_LOG+x}

restore_user_manager_environment() {
    command -v systemctl >/dev/null 2>&1 || return 0
    systemctl --user set-environment "PATH=$ORIGINAL_PATH" >/dev/null 2>&1 || true
    if [ -n "$ORIGINAL_XDG_DATA_DIRS_SET" ]; then
        systemctl --user set-environment "XDG_DATA_DIRS=$ORIGINAL_XDG_DATA_DIRS" >/dev/null 2>&1 || true
    else
        systemctl --user unset-environment XDG_DATA_DIRS >/dev/null 2>&1 || true
    fi
    if [ -n "$ORIGINAL_XDG_CONFIG_HOME_SET" ]; then
        systemctl --user set-environment "XDG_CONFIG_HOME=$ORIGINAL_XDG_CONFIG_HOME" >/dev/null 2>&1 || true
    else
        systemctl --user unset-environment XDG_CONFIG_HOME >/dev/null 2>&1 || true
    fi
    if [ -n "$ORIGINAL_RUST_LOG_SET" ]; then
        systemctl --user set-environment "RUST_LOG=$ORIGINAL_RUST_LOG" >/dev/null 2>&1 || true
    else
        systemctl --user unset-environment RUST_LOG >/dev/null 2>&1 || true
    fi
    systemctl --user unset-environment \
        COSMIC_SCROLLING_TILING COSMIC_SCROLLING_SESSION >/dev/null 2>&1 || true
}
trap restore_user_manager_environment EXIT

export COSMIC_SCROLLING_TILING=1
export COSMIC_SCROLLING_SESSION=1
export PATH="$PRIVATE_PREFIX/bin:$PROJECT_ROOT/target/debug:$PATH"
export XDG_DATA_DIRS="$PRIVATE_PREFIX/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export RUST_LOG="${RUST_LOG:-cosmic_comp=info}"
export XDG_CONFIG_HOME="$TEST_CONFIG_HOME"

# Skip start-cosmic's login-shell recursion so the development PATH above is
# preserved when cosmic-session launches cosmic-comp. Keep this shell as the
# parent so its EXIT trap can restore the user manager environment on logout.
/usr/bin/start-cosmic --in-login-shell

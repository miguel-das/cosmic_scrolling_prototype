#!/bin/sh

set -eu

OWNER_ID=cosmic-scrolling-prototype-v1
UPSTREAM_URL=https://github.com/pop-os/cosmic-applets.git
SYSTEM_LAUNCHER=/usr/local/bin/cosmic-scrolling-test-session
SYSTEM_DESKTOP=/usr/share/wayland-sessions/cosmic-scrolling-test.desktop

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '%s\n' "$*"
}

need_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

run_as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

safe_remove_tree() {
    candidate=$1
    case "$candidate" in
        "$STATE_ROOT"/*) ;;
        *) die "refusing to remove path outside the installer state directory: $candidate" ;;
    esac
    [ "$candidate" != "$STATE_ROOT" ] || die "refusing to remove the installer state root"
    rm -rf -- "$candidate"
}

INSTALL_SESSION=true
case "${1:-}" in
    -h|--help)
        cat <<'EOF'
Usage: ./install.sh [--build-only]

Build and install the isolated COSMIC Scrolling Test login session.

  --build-only        Build both programs and refresh the private applet prefix
                      without installing or changing system greeter files.

Environment:
  COSMIC_APPLETS_REV  Exact cosmic-applets Git revision. If unset, the final
                      component of the installed cosmic-applets package version
                      is used.
EOF
        exit 0
        ;;
    --build-only) INSTALL_SESSION=false ;;
    '') ;;
    *) die "unknown argument: $1 (try --help)" ;;
esac
[ "$#" -le 1 ] || die "too many arguments (try --help)"

need_command readlink
SCRIPT_PATH=$(readlink -f -- "$0") || die "cannot resolve the installer path"
SUITE_ROOT=$(CDPATH= cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd -P)
COMP_ROOT="$SUITE_ROOT/cosmic-comp-scrolling-prototype"
APPLET_SOURCE="$SUITE_ROOT/cosmic-ext-applet-scrolling-tiling"
STATE_ROOT="$SUITE_ROOT/.cosmic-scrolling"
PREFIX="$STATE_ROOT/prefix"
UPSTREAM_REPO="$STATE_ROOT/upstream-cosmic-applets"
BUILD_WORKSPACE="$STATE_ROOT/build-workspace"
CARGO_TARGET="$STATE_ROOT/cargo-target"
SOURCE_LAUNCHER="$COMP_ROOT/start-scrolling-session.sh"
SOURCE_DESKTOP="$COMP_ROOT/cosmic-scrolling-test.desktop"

[ "$(id -u)" -ne 0 ] || die "run this installer as your normal desktop user, not with sudo"

for command_name in awk basename cargo cat chmod cp git grep install ln mkdir mktemp mv rm tar; do
    need_command "$command_name"
done
if [ "$INSTALL_SESSION" = true ]; then
    need_command sudo
fi

[ -f "$COMP_ROOT/Cargo.toml" ] || die "missing compositor source: $COMP_ROOT"
[ -f "$COMP_ROOT/PROJECT_HANDOFF.md" ] || die "missing compositor handoff file"
[ -f "$APPLET_SOURCE/Cargo.toml" ] || die "missing applet source: $APPLET_SOURCE"
[ -f "$APPLET_SOURCE/data/com.system76.CosmicAppletTiling.desktop" ] \
    || die "missing applet desktop entry"
[ -d "$APPLET_SOURCE/data/icons/scalable/apps" ] || die "missing applet icons"
[ -f "$SOURCE_LAUNCHER" ] || die "missing session launcher: $SOURCE_LAUNCHER"
[ -f "$SOURCE_DESKTOP" ] || die "missing session desktop entry: $SOURCE_DESKTOP"

if [ -e "$STATE_ROOT/manifest" ]; then
    grep -q "^owner=$OWNER_ID\$" "$STATE_ROOT/manifest" \
        || die "refusing to use state owned by another installer: $STATE_ROOT"
elif [ -e "$PREFIX/bin/cosmic-applet-tiling" ] \
    || [ -e "$PREFIX/share/applications/com.system76.CosmicAppletTiling.desktop" ]; then
    die "refusing to overwrite an unowned private applet prefix: $PREFIX"
fi

case "${COSMIC_APPLETS_REV:-}" in
    '')
        need_command dpkg-query
        PACKAGE_VERSION=$(dpkg-query -W -f='${Version}\n' cosmic-applets 2>/dev/null) \
            || die "cannot read the installed cosmic-applets version; set COSMIC_APPLETS_REV"
        APPLETS_REV=${PACKAGE_VERSION##*~}
        case "$APPLETS_REV" in
            ''|*[!0-9a-fA-F]*)
                die "cannot derive a Git revision from cosmic-applets version '$PACKAGE_VERSION'; set COSMIC_APPLETS_REV"
                ;;
        esac
        ;;
    *) APPLETS_REV=$COSMIC_APPLETS_REV ;;
esac

case "$APPLETS_REV" in
    -*|'') die "invalid cosmic-applets revision: $APPLETS_REV" ;;
esac

mkdir -p -- "$STATE_ROOT" "$PREFIX/bin" "$PREFIX/share/applications" \
    "$PREFIX/share/icons/hicolor/scalable/apps" "$CARGO_TARGET"

note "Building the scrolling compositor..."
(cd -- "$COMP_ROOT" && cargo build --locked)
COMPOSITOR="$COMP_ROOT/target/debug/cosmic-comp"
[ -x "$COMPOSITOR" ] || die "Cargo completed but the compositor is missing: $COMPOSITOR"

if [ ! -d "$UPSTREAM_REPO/.git" ]; then
    [ ! -e "$UPSTREAM_REPO" ] \
        || die "upstream cache exists but is not a Git clone: $UPSTREAM_REPO"
    note "Cloning the official cosmic-applets workspace..."
    git clone --filter=blob:none --no-checkout "$UPSTREAM_URL" "$UPSTREAM_REPO"
else
    CACHE_REMOTE=$(git -C "$UPSTREAM_REPO" remote get-url origin 2>/dev/null || true)
    case "$CACHE_REMOTE" in
        https://github.com/pop-os/cosmic-applets|https://github.com/pop-os/cosmic-applets.git|git@github.com:pop-os/cosmic-applets.git)
            ;;
        *) die "refusing to use an unexpected upstream cache remote: ${CACHE_REMOTE:-<missing>}" ;;
    esac
fi

note "Selecting cosmic-applets revision $APPLETS_REV..."
git -C "$UPSTREAM_REPO" fetch origin
if RESOLVED_REV=$(git -C "$UPSTREAM_REPO" rev-parse --verify "$APPLETS_REV^{commit}" 2>/dev/null); then
    :
else
    git -C "$UPSTREAM_REPO" fetch --depth 1 origin "$APPLETS_REV" \
        || die "cannot resolve cosmic-applets revision '$APPLETS_REV'; use a full official commit or tag"
    RESOLVED_REV=$(git -C "$UPSTREAM_REPO" rev-parse --verify 'FETCH_HEAD^{commit}')
fi
git -C "$UPSTREAM_REPO" switch --detach "$RESOLVED_REV"

STAGING_WORKSPACE=$(mktemp -d "$STATE_ROOT/build-workspace.new.XXXXXX")
ARCHIVE_FILE="$STATE_ROOT/cosmic-applets-$RESOLVED_REV.tar"
cleanup_staging() {
    [ -n "${STAGING_WORKSPACE:-}" ] && [ ! -e "$STAGING_WORKSPACE" ] \
        || safe_remove_tree "$STAGING_WORKSPACE"
    rm -f -- "$ARCHIVE_FILE"
}
trap cleanup_staging EXIT HUP INT TERM

git -C "$UPSTREAM_REPO" archive --format=tar --output="$ARCHIVE_FILE" HEAD
tar -xf "$ARCHIVE_FILE" -C "$STAGING_WORKSPACE"
rm -f -- "$ARCHIVE_FILE"

# Resolve only the applet being built. Keeping every unrelated applet as an
# active workspace member makes Cargo recalculate feature combinations for the
# entire historical workspace when the config path dependency is substituted.
WORKSPACE_MANIFEST="$STAGING_WORKSPACE/Cargo.toml"
awk '
    /^default-members = / {
        print "default-members = [\"cosmic-applet-tiling\"]"
        next
    }
    /^members = \[/ {
        print "members = [\"cosmic-applet-tiling\"]"
        in_members = 1
        next
    }
    in_members {
        if ($0 ~ /^\]/) in_members = 0
        next
    }
    { print }
' "$WORKSPACE_MANIFEST" >"$WORKSPACE_MANIFEST.new"
mv -- "$WORKSPACE_MANIFEST.new" "$WORKSPACE_MANIFEST"
grep -q '^members = \["cosmic-applet-tiling"\]$' "$WORKSPACE_MANIFEST" \
    || die "failed to limit the assembled applet workspace"

UPSTREAM_APPLET="$STAGING_WORKSPACE/cosmic-applet-tiling"
[ -d "$UPSTREAM_APPLET" ] \
    || die "the selected cosmic-applets workspace has no cosmic-applet-tiling member"
safe_remove_tree "$UPSTREAM_APPLET"
mkdir -p -- "$UPSTREAM_APPLET"
cp -a -- "$APPLET_SOURCE/." "$UPSTREAM_APPLET/"

if [ -e "$STAGING_WORKSPACE/cosmic-comp" ] || [ -L "$STAGING_WORKSPACE/cosmic-comp" ]; then
    die "the selected workspace unexpectedly contains a cosmic-comp path"
fi
mkdir -p -- "$STAGING_WORKSPACE/cosmic-comp/cosmic-comp-config"
cp -a -- "$COMP_ROOT/cosmic-comp-config/." \
    "$STAGING_WORKSPACE/cosmic-comp/cosmic-comp-config/"

# The applet consumes only the config crate's default API. Exclude newer
# compositor-only optional dependencies from this copied build manifest: they
# can otherwise introduce a second, incompatible COSMIC UI dependency graph
# into an older package-matched applet workspace.
CONFIG_MANIFEST="$STAGING_WORKSPACE/cosmic-comp/cosmic-comp-config/Cargo.toml"
cat >"$CONFIG_MANIFEST" <<'EOF'
[package]
name = "cosmic-comp-config"
version = "0.1.0"
edition = "2024"

[dependencies]
cosmic-config = { git = "https://github.com/pop-os/libcosmic/" }
input = "0.9.0"
serde = { version = "1", features = ["derive"] }

[features]
default = []
output = []
libdisplay-info = []
randr = []
EOF

# Detach the obsolete Git source from the one lock entry that the local path
# package replaces. Leaving both identities present during `cargo update` can
# make Cargo re-resolve an incompatible historical iced feature combination.
LOCK_FILE="$STAGING_WORKSPACE/Cargo.lock"
LOCKED_CONFIG_SOURCES=$(awk '
    /^\[\[package\]\]$/ { in_config = 0 }
    /^name = "cosmic-comp-config"$/ { in_config = 1 }
    in_config && index($0, "source = \"git+https://github.com/pop-os/cosmic-comp.git") == 1 { count++ }
    END { print count + 0 }
' "$LOCK_FILE")
[ "$LOCKED_CONFIG_SOURCES" -eq 1 ] \
    || die "the selected lockfile has an unexpected cosmic-comp-config source"
awk '
    /^\[\[package\]\]$/ { in_config = 0 }
    /^name = "cosmic-comp-config"$/ { in_config = 1 }
    in_config && index($0, "source = \"git+https://github.com/pop-os/cosmic-comp.git") == 1 { next }
    { print }
' "$LOCK_FILE" >"$LOCK_FILE.new"
mv -- "$LOCK_FILE.new" "$LOCK_FILE"

note "Building the modified Window Layout applet..."
# The upstream lock records a Git-sourced cosmic-comp-config 0.1.0, while this
# project intentionally substitutes the modified source as a path package.
# Reconcile only that package in the disposable workspace, then require the
# actual build to honor the resulting lock exactly.
CARGO_TARGET_DIR="$CARGO_TARGET" cargo update \
    --manifest-path "$STAGING_WORKSPACE/Cargo.toml" \
    -p cosmic-comp-config
CARGO_TARGET_DIR="$CARGO_TARGET" cargo build --locked \
    --manifest-path "$STAGING_WORKSPACE/Cargo.toml" \
    -p cosmic-applet-tiling
APPLET_BINARY="$CARGO_TARGET/debug/cosmic-applet-tiling"
[ -x "$APPLET_BINARY" ] || die "Cargo completed but the applet is missing: $APPLET_BINARY"

if [ -e "$BUILD_WORKSPACE" ] || [ -L "$BUILD_WORKSPACE" ]; then
    safe_remove_tree "$BUILD_WORKSPACE"
fi
mv -- "$STAGING_WORKSPACE" "$BUILD_WORKSPACE"
STAGING_WORKSPACE=
trap - EXIT HUP INT TERM

install -m 0755 "$APPLET_BINARY" "$PREFIX/bin/cosmic-applet-tiling"
# This path is relative to .cosmic-scrolling/prefix/bin, keeping the symlink
# portable and free of checkout/user-specific path information.
ln -sfn -- ../../../cosmic-comp-scrolling-prototype/target/debug/cosmic-comp \
    "$PREFIX/bin/cosmic-comp"
PRIVATE_DESKTOP="$PREFIX/share/applications/com.system76.CosmicAppletTiling.desktop"
# COSMIC applets are hidden panel plugins and do not need an application-menu
# category. The upstream `Categories=COSMIC;` value is not registered by the
# freedesktop specification, so omit it from this private validated copy.
grep -v '^Categories=COSMIC;$' \
    "$APPLET_SOURCE/data/com.system76.CosmicAppletTiling.desktop" \
    >"$PRIVATE_DESKTOP.new"
mv -- "$PRIVATE_DESKTOP.new" "$PRIVATE_DESKTOP"
chmod 0644 "$PRIVATE_DESKTOP"
for icon in "$APPLET_SOURCE"/data/icons/scalable/apps/com.system76.CosmicAppletTiling*.svg; do
    [ -f "$icon" ] || die "expected applet icon is missing"
    install -m 0644 "$icon" "$PREFIX/share/icons/hicolor/scalable/apps/$(basename -- "$icon")"
done

cat >"$STATE_ROOT/manifest" <<EOF
owner=$OWNER_ID
cosmic_applets_revision=$RESOLVED_REV
compositor=cosmic-comp-scrolling-prototype/target/debug/cosmic-comp
applet=.cosmic-scrolling/prefix/bin/cosmic-applet-tiling
launcher=cosmic-comp-scrolling-prototype/start-scrolling-session.sh
EOF

chmod 0755 "$SOURCE_LAUNCHER"

if [ "$INSTALL_SESSION" = false ]; then
    note ""
    note "Build completed without changing system greeter files."
    note "  compositor: $COMPOSITOR"
    note "  applet:     $PREFIX/bin/cosmic-applet-tiling"
    note "  revision:   $RESOLVED_REV"
    note "Run ./install.sh when you are ready to install the login session."
    exit 0
fi

if [ -e "$SYSTEM_LAUNCHER" ] || [ -L "$SYSTEM_LAUNCHER" ]; then
    if [ -L "$SYSTEM_LAUNCHER" ]; then
        OLD_TARGET=$(readlink "$SYSTEM_LAUNCHER" 2>/dev/null || true)
        case "$OLD_TARGET" in
            "$SOURCE_LAUNCHER"|*/start-scrolling-session.sh) ;;
            *) die "refusing to replace unrelated symlink $SYSTEM_LAUNCHER -> ${OLD_TARGET:-<unreadable>}" ;;
        esac
    else
        die "refusing to replace non-symlink path: $SYSTEM_LAUNCHER"
    fi
fi

if [ -e "$SYSTEM_DESKTOP" ]; then
    if grep -q "^X-CosmicScrollingOwner=$OWNER_ID\$" "$SYSTEM_DESKTOP" 2>/dev/null; then
        :
    elif grep -q '^Name=COSMIC Scrolling Test$' "$SYSTEM_DESKTOP" 2>/dev/null; then
        note "Migrating the existing legacy COSMIC Scrolling Test desktop entry."
    else
        die "refusing to replace unrelated desktop entry: $SYSTEM_DESKTOP"
    fi
fi

note "Installing the greeter entry (administrator authentication may be requested)..."
run_as_root install -d -m 0755 "$(dirname -- "$SYSTEM_LAUNCHER")"
run_as_root ln -sfn "$SOURCE_LAUNCHER" "$SYSTEM_LAUNCHER"
run_as_root install -D -m 0644 "$SOURCE_DESKTOP" "$SYSTEM_DESKTOP"

note ""
note "Installed COSMIC Scrolling Test."
note "  compositor: $COMPOSITOR"
note "  applet:     $PREFIX/bin/cosmic-applet-tiling"
note "  revision:   $RESOLVED_REV"
note "  session:    $SYSTEM_DESKTOP"
note ""
note "Log out, choose 'COSMIC Scrolling Test' in the greeter, and log in."
note "Run ./install.sh again after moving this directory or rebuilding either project."

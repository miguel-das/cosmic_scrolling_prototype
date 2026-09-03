# COSMIC scrolling test session

This repository can install an additional greeter entry named **COSMIC
Scrolling Test**. It runs `target/debug/cosmic-comp` from the clone where the
installer was invoked, with
`COSMIC_SCROLLING_TILING=1`. The normal COSMIC session and
`/usr/bin/cosmic-comp` are not replaced. Its COSMIC configuration is isolated
in `target/scrolling-test-config`, so it does not change your normal COSMIC
settings.

No repository path is hardcoded. The installer creates
`/usr/local/bin/cosmic-scrolling-test-session` as a symlink to this clone's
launcher. The launcher resolves that symlink to find the clone. If the clone is
moved, rerun the installer from its new location.

## Build dependencies

Install the native libraries this compositor links against (from
`debian/control`):

```bash
sudo apt install \
    cmake \
    libegl1-mesa-dev \
    libfontconfig-dev \
    libgbm-dev \
    libinput-dev \
    libpixman-1-dev \
    libseat-dev \
    libsystemd-dev \
    libudev-dev \
    libwayland-dev \
    libxcb1-dev \
    libxkbcommon-dev \
    libdisplay-info-dev
```

You also need a Rust toolchain new enough for `rust-version = "1.93"`
(`Cargo.toml`) and the 2024 edition. Debian/Ubuntu's packaged `rustc`/`cargo`
are normally far too old for this; install a current toolchain with
[rustup](https://rustup.rs) instead:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Build and install

```bash
# Run from the root of your cosmic-comp clone.
cargo test --locked scrolling::tests
cargo build --locked
chmod +x start-scrolling-session.sh install-scrolling-session.sh
./install-scrolling-session.sh
```

Building only creates `target/debug/cosmic-comp`; it does not install a login
session. The installer adds the stable launcher symlink and the greeter entry.
It may request administrator authentication.

After later code changes, only `cargo build --locked` is required. Reinstall
only after moving the clone or changing the session files.

## Start the session

1. Save your work and log out of the current desktop session.
2. Open the session chooser on the login screen.
3. Select **COSMIC Scrolling Test**.
4. Log in normally.

The test session uses the physical keyboard directly, so the outer desktop
cannot intercept `Super` shortcuts. Open four terminals with `Super+T`, then
test:

```text
Super+Left / Super+Right
Super+Shift+Left / Super+Shift+Right
Super+Ctrl+Up / Super+Ctrl+Down
```

Do not set `COSMIC_BACKEND=winit` for this login session. Automatic backend
selection uses the normal KMS backend.

## Switch tiling mode from the CLI

Run these commands from a terminal inside **COSMIC Scrolling Test**. The test
launcher sets `XDG_CONFIG_HOME` to this clone's isolated configuration, so the
commands do not modify the normal COSMIC session.

Switch immediately to Scrolling mode:

```bash
MODE_FILE="$XDG_CONFIG_HOME/cosmic/com.system76.CosmicComp/v1/tiling_engine"
printf '%s\n' Scrolling >"$MODE_FILE"
```

Switch immediately to Classic mode:

```bash
MODE_FILE="$XDG_CONFIG_HOME/cosmic/com.system76.CosmicComp/v1/tiling_engine"
printf '%s\n' Classic >"$MODE_FILE"
```

For repeated testing, define a helper once in that terminal:

```bash
MODE_FILE="$XDG_CONFIG_HOME/cosmic/com.system76.CosmicComp/v1/tiling_engine"
set_tiling_mode() {
    case "$1" in
        Classic|Scrolling) printf '%s\n' "$1" >"$MODE_FILE" ;;
        *) printf 'Usage: set_tiling_mode Classic|Scrolling\n' >&2; return 2 ;;
    esac
}
```

Then use:

```bash
set_tiling_mode Scrolling
set_tiling_mode Classic
```

Values are case-sensitive and must be exactly `Classic` or `Scrolling`. The
running compositor watches this setting, so logging out or restarting it is not
required. Check the active configured value with `cat "$MODE_FILE"`.

## Remove the test session

First log into the normal COSMIC session. Then run:

```bash
sudo rm -f /usr/share/wayland-sessions/cosmic-scrolling-test.desktop
sudo rm -f /usr/local/bin/cosmic-scrolling-test-session
```

That removes the greeter entry. The normal
`/usr/share/wayland-sessions/cosmic.desktop` entry is not touched.

Remove settings created by the test session:

```bash
# Run from the root of the clone used for testing.
rm -rf target/scrolling-test-config
```

To remove build output produced while testing:

```bash
# Run from the root of the clone used for testing.
cargo clean
```

`cargo clean` deletes the entire repository `target/` directory, including
unrelated cached build artifacts. It is optional and can require a full rebuild
next time.

## Check for leftovers

Verify that the greeter entry is gone:

```bash
test ! -e /usr/share/wayland-sessions/cosmic-scrolling-test.desktop \
  && echo "Test session entry removed"
test ! -e /usr/local/bin/cosmic-scrolling-test-session \
  && echo "Test session launcher removed"
```

Check for stale sockets left by previous nested/Winit tests:

```bash
find "$XDG_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -print
find /tmp/.X11-unix -maxdepth 1 -type s -name 'X*' -print 2>/dev/null
```

Do not delete sockets that have a live compositor or X server attached. The
dedicated login session normally cleans up its sockets automatically during
logout.

## Recovery

If the test session fails to start, return to the greeter and choose the normal
**COSMIC** session. If necessary, switch to a text console with
`Ctrl+Alt+F3`, log in, and remove the entry:

```bash
sudo rm -f /usr/share/wayland-sessions/cosmic-scrolling-test.desktop
sudo rm -f /usr/local/bin/cosmic-scrolling-test-session
```

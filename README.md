# COSMIC Scrolling prototype

> [!WARNING]
> **Unmaintained proof of concept.** Not a System76 project, the COSMIC
> developers will not support it, and neither will I. No issues, no fixes, no
> guarantees. Feel free to **Fork it, refactor it, update it.**
>
> **Written with GPT 5.6 Sol.** This code was AI-generated step by step and tested by myself.

## Demo

[▶ Watch the demo](./demo.mp4)

This project installs a separate **COSMIC Scrolling Test** login session using:

- the scrolling compositor from `cosmic-comp-scrolling-prototype`; and
- the modified Window Layout applet with **Floating**, **Tiling**, and
  **Scrolling** modes.

It does not replace the normal COSMIC compositor, applet, login session, or
panel configuration.

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

## Install

Requirements: Pop!_OS/COSMIC, Git, Cargo/Rust, COSMIC build dependencies,
network access, and `sudo` authentication for the greeter entry.

Run as your normal user, not with `sudo`:

```bash
cd /path/to/project
chmod +x install.sh uninstall.sh
./install.sh
```

The installer detects the compatible `cosmic-applets` Git revision from the
installed package. If detection fails, provide it explicitly:

```bash
COSMIC_APPLETS_REV=ab9d069 ./install.sh
```

To build both programs without installing the greeter entry:

```bash
./install.sh --build-only
```

After installation, log out, select **COSMIC Scrolling Test** in the session
chooser, and log in. Run `./install.sh` again after source changes or after
moving the project directory.

## Layout controls

The Window Layout applet provides:

- **Floating** — disables tiling on the current workspace.
- **Tiling** — enables COSMIC's Classic tiling engine.
- **Scrolling** — enables the horizontal scrolling tiling engine.

Scrolling keyboard and touchpad controls:

- `Super+Left/Right` — focus the neighboring column.
- `Super+Up/Down` — focus a tile vertically or move between workspaces at a
  column edge.
- `Super+Shift+Left/Right` — extract or move a column.
- `Super+Shift+Up/Down` — reorder, merge, or move a tile between workspaces.
- `Super+C` — center the focused column.
- `Super+R` / `Super+Shift+R` — cycle column widths through 33%, 50%, 66%, and
  100%.
- Three-finger horizontal touchpad movement — pan the scrolling strip.

Pointer window placement and horizontal or vertical mouse resizing are also
supported.

To switch engines from a terminal inside the test session:

```bash
MODE_FILE="$XDG_CONFIG_HOME/cosmic/com.system76.CosmicComp/v1/tiling_engine"
printf '%s\n' Scrolling >"$MODE_FILE"
printf '%s\n' Classic >"$MODE_FILE"
```

## Verify

Inside the test session, these paths should resolve inside this project rather
than `/usr/bin`:

```bash
readlink -f "$(command -v cosmic-comp)"
readlink -f "$(command -v cosmic-applet-tiling)"
```

## Uninstall

Log into normal **COSMIC**, then run:

```bash
cd /path/to/project
./uninstall.sh
```

Preserve build files but also remove the isolated test-session settings with:

```bash
./uninstall.sh --purge-config
```

If the test session fails, return to the normal **COSMIC** session from the
greeter or press `Ctrl+Alt+F3`, log in, and run the uninstaller.

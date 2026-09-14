# COSMIC Scrolling prototype

> [!WARNING]
> **Unmaintained proof of concept.** Not a System76 project, the COSMIC
> developers will not support it, and neither will I. No issues, no fixes, no
> guarantees. Feel free to **Fork it, refactor it, update it.**
>
> **Written with GPT 5.6 Sol.** This code was AI-generated step by step and tested by myself.

## Demo

https://github.com/user-attachments/assets/8e0f6924-a481-40ec-9e68-0179c21bd718

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
COSMIC_APPLETS_REV='matching-commit' ./install.sh
```

The applet is assembled against that upstream workspace in `.cosmic-scrolling/`.
Its config source is copied from **`cosmic-comp-scrolling-prototype`**, not the
unmodified `cosmic-comp` reference folder. The installer tests the applet,
then builds both programs with locked dependencies. Translations are embedded
in the applet binary so it works after the temporary workspace is moved. The applet's assembled
lockfile is reconciled for this local config dependency first.

To build both programs and refresh the private applet without changing the greeter:

```bash
./install.sh --build-only
```

After installation, log out, select **COSMIC Scrolling Test** in the session
chooser, and log in. Run `./install.sh` again after source changes or after
moving the project directory.

For a staged installation test without administrator access:

```bash
./install.sh --destdir /tmp/cosmic-scrolling-stage
./uninstall.sh --destdir /tmp/cosmic-scrolling-stage
```

Only greeter files are redirected by `--destdir` (or `DESTDIR`). Builds and the
private applet remain in this project; staged uninstall removes that private
applet too. Staging does not add a live login option.

The session launcher prepends `.cosmic-scrolling/prefix/bin` and its `share`
directory to the test session's search paths. This makes the existing tiling
panel entry use the modified applet. Paths are restored in the user systemd
manager on logout. Keep this checkout in place while it is installed.

## Layout controls

The Window Layout applet provides:

- **Floating** — disables tiling on the current workspace.
- **Tiling** — enables COSMIC's Classic tiling engine.
- **Scrolling** — enables the horizontal scrolling tiling engine.

**The engine choice is global:** Tiling/Scrolling changes every tiled
workspace. Floating changes only the active workspace and preserves the
selected engine. The separate **New workspace behavior** control chooses
Tiled/Floating defaults; it does not select an engine.

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

## Review and validation

The compositor's Classic width-animation correction remains restricted to
Scrolling; Classic preserves upstream rendering behavior. The applet uses the
same `tiling_engine` setting and workspace protocol as this prototype.

Automated applet layout tests run during installation. Script checks can be
run without a live session:

```bash
python3 -m unittest discover -s tests -v
python3 -m unittest discover -s cosmic-comp-scrolling-prototype/tests -v
```

Before relying on a new build, check Floating/Tiling/Scrolling, workspace
switches, the three icons, and logout back into normal COSMIC. See the
[applet checks](cosmic-ext-applet-scrolling-tiling/README.md#manual-checks).
Also check compositor focus, window placement, resizing, and engine transitions.
Building and staged installation do not validate a physical greeter login,
multi-monitor hotplug, or touchpad behavior.

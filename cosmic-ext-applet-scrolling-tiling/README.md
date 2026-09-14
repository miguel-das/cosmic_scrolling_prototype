# COSMIC Window Layout applet development

This source-only package comes from the `cosmic-applets` 1.0.15 workspace. Its
`workspace = true` dependencies require the matching upstream workspace and
lockfile; running Cargo directly in this directory is not a supported build.

For development, match the exact upstream revision used by the installed
`cosmic-applets` package, not only its semantic version. Pop!_OS may publish
multiple dependency/API updates under the same 1.0.15 version. The final short
hash in the package version identifies the matching `cosmic-applets` commit:

```bash
dpkg-query -W -f='${Version}\n' cosmic-applets
```

From the parent project, use the supported assembly/build procedure:

```bash
./install.sh --build-only
```

It copies this applet into a disposable matching upstream workspace and takes
`cosmic-comp-config` from `cosmic-comp-scrolling-prototype`. It narrows the
copied config manifest to the default API needed by the applet, reconciles the
lockfile for that path dependency, then runs tests and a locked build. Neither
the reference `cosmic-comp` folder nor your original applet sources are rewritten.
The assembled workspace and lockfile remain in `.cosmic-scrolling/build-workspace`.
Rerun the installer after editing this source; do not edit that generated copy.

The relative `../cosmic-comp/cosmic-comp-config` dependency refers to the
**assembled workspace**, not the parent's upstream reference checkout.
Before upstreaming or packaging, use a committed dependency revision containing
`TilingEngine` and provide a reproducible workspace/lockfile.

The suite installs the default binary in `.cosmic-scrolling/prefix`, visible
only through the isolated session's search paths. Run `../uninstall.sh` from
this directory to remove that installation; normal COSMIC remains untouched.

The technical package name, binary, desktop ID, and icon namespace remain
`cosmic-applet-tiling` / `com.system76.CosmicAppletTiling` for compatibility
with existing COSMIC panel configurations.

For a separate, manually managed parallel development installation that does not replace the packaged
tiling applet, build with `--features parallel-test-install`. Install the
result under the separate executable and desktop identity:

```text
cosmic-applet-window-layout-test
com.system76.CosmicAppletWindowLayoutTest
```

The parallel build also expects the corresponding
`com.system76.CosmicAppletWindowLayoutTest.*` icon names. Keep all of these in
the user-local `~/.local` prefix; do not overwrite `/usr/bin/cosmic-applet-tiling`
or `/usr/share/applications/com.system76.CosmicAppletTiling.desktop`.
If the desktop session does not include `~/.local/bin` in `PATH`, resolve the
installed desktop entry's `Exec` value to that user's absolute local-bin path
during installation; do not store that machine-specific path in this source.

Remove only the parallel development installation with:

```bash
rm -f ~/.local/bin/cosmic-applet-window-layout-test
rm -f ~/.local/share/applications/com.system76.CosmicAppletWindowLayoutTest.desktop
rm -f ~/.local/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletWindowLayoutTest-symbolic.svg
rm -f ~/.local/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletWindowLayoutTest.Off.svg
rm -f ~/.local/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletWindowLayoutTest.On.svg
rm -f ~/.local/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletWindowLayoutTest.Scrolling.svg
```

## Manual checks

Run the applet with the modified compositor, then verify:

1. **Floating** makes only the active workspace floating and does not change
   the `tiling_engine` configuration value.
2. **Tiling** enables tiling on the active workspace and writes `Classic` to
   `com.system76.CosmicComp`'s `tiling_engine` entry.
3. **Scrolling** enables tiling on the active workspace and writes `Scrolling`
   to that same global entry.
4. Moving between workspaces updates the selector from each workspace's tiling
   state combined with the one global engine value.
5. Changing the engine updates every already-tiled workspace while floating
   workspaces continue to display **Floating**.
6. The separate new-workspace **Tiled/Floating** choice still changes only the
   default tiling state; a tiled new workspace uses the selected global engine.
7. The panel icon distinguishes Floating, Classic Tiling, and Scrolling, while
   active-hint and window-management controls continue to work.

8. Rapid mode clicks must not leave the engine at an earlier requested mode;
   the selector is temporarily disabled while its config write completes.
   A failed engine write must not enable workspace tiling or show a false mode.
9. On multiple outputs, the applet must control its own panel output. Recheck
   after output removal/reconnection; it must never fall back to another output.

The alternate `parallel-test-install` feature is not installed or removed by
the suite scripts. Its manual files above are a separate development option.

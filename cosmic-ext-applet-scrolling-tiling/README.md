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

Check out that official revision and place this package at its
`cosmic-applet-tiling/` member path. Make the modified `cosmic-comp` clone
available as `cosmic-comp/` beside that package inside the temporary workspace
(a local symlink is sufficient), then run:

```bash
cargo test --locked -p cosmic-applet-tiling
cargo build --locked -p cosmic-applet-tiling
```

The relative `../cosmic-comp/cosmic-comp-config` dependency is intentional for
the current sibling-source development layout and contains the new
`TilingEngine` setting. Before upstreaming or packaging this applet, replace it
with a published version or a committed `cosmic-comp` revision that includes
`TilingEngine`; do not publish a package that depends on a local path.

The technical package name, binary, desktop ID, and icon namespace remain
`cosmic-applet-tiling` / `com.system76.CosmicAppletTiling` for compatibility
with existing COSMIC panel configurations.

For a parallel development installation that does not replace the packaged
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

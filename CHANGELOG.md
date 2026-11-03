# Unity Editor UI Scalar for Linux — changes

## Unreleased

- `packaging/install-deps.sh` now resolves release-specific package names
  (`kscreen6`/`kscreen5`, `xorg-x11-server-Xwayland`/`xwayland`,
  `pkgconf-pkg-config`/`pkgconf`), installs a C compiler for the bridge,
  adds `--check`, and re-verifies the install so an unsupported
  distribution reports the missing GTK/libadwaita version instead of
  failing later. CI runs the installer on Ubuntu.
- `./install.sh --with-dependencies` installs the distribution packages for
  Debian/Ubuntu, Fedora, openSUSE and Arch, builds the bridge when it is missing,
  then installs the app. `packaging/install-deps.sh` can be used on its own and
  supports `--runtime-only`, `--build-only`, `--dry-run` and `--yes`.
- Unity Hub installed as a Flatpak now works: the generated Editor launcher
  re-runs itself on the host through `flatpak-spawn --host`, because Hub's
  sandbox has no `Xwayland`, no `kscreen-doctor` and a glibc older than the
  native helper. Re-enable an existing override once to regenerate the launcher.
- `-projectpath` from Unity Hub is recognized in any letter case.
- Bridge: a toplevel that uses `WM_TAKE_FOCUS` (Unity, and most GTK/Qt
  applications) never became the remembered focus target, so every focus
  request from the Editor and its menu popups was dropped. Menu buttons
  therefore did not open on the first click. The focused toplevel is now
  recorded for both focus protocols.
- Bridge: popup geometry is now always relative to the popup's parent window,
  never to the output origin. Creation, client repositions and compositor
  configures share one origin, which removes the transient flash at the far
  right of the screen when a menu is first shown away from the output origin.
- Bridge: a re-mapped menu popup could stay invisible. Xwayland announces a
  window's surface serial either before or after the window's own serial, and
  the bridge only matched the window-first order. A surface that arrived
  first kept a buffer but never received a role, so the mapped, painted menu
  was never presented by the compositor. The association now completes in
  either order.
- Bridge: the deferred popup pointer enter was also replayed before a button
  *release*. That delivers an `EnterNotify` to the new popup inside the
  client's popup-activation interval, which makes GTK-family menus (Unity
  included) tear the menu down, so a menu click only worked sometimes. The
  deferred enter is now replayed before presses only.
- Bridge: pointer button and deferred-enter events are logged at debug level.
- Bridge: a popup created before its own `wl_surface` enter event used the X
  root origin instead of its parent's output origin, so Unity's menu bar popups
  were placed at the right edge of the screen and needed repeated clicks.
  Popups now inherit the parent window's output origin at creation, which also
  keeps later repositions stable. `UNITY_SCALE_BRIDGE_LOG=debug` raises the
  helper log level for this kind of diagnosis.

## 0.1.0-beta.1

- Standalone GTK4/libadwaita interface with explicit installed Editor scan and expandable per-Editor controls.
- Editor-wide scale profiles with automatic saving, a numeric scale control, collapsed scale summaries and pending-edit confirmation; no project configuration or mandatory project picker.
- Reversible Unity Hub launch integration and command-line argument forwarding.
- User-local install/uninstall and external diagnostics/log locations.
- Includes bridge prototype-v5.2: fixed render density, independent UI scale,
  cursor consistency, exact native popup allocation and EWMH docking support.

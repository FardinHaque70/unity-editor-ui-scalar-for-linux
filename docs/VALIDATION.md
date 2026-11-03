# Beta validation

## Bridge prototype-v5.3

-113 library regression tests passed, including popup repeated configure,
 window-list lifecycle, cursor, viewport, output, focus and input protocol tests.
 v5.3 adds `take_focus_toplevel_is_still_the_focus_restore_target`,
 `popup_uses_parent_output_origin_before_its_own_enter`,
 `popup_offset_is_relative_to_its_parent_window` and
 `surface_serial_before_window_serial_still_associates`. The last two fail on the
 v5.2 sources, so they guard the reported defects rather than describing them.
- Interactive menu validation, openSUSE Tumbleweed, KDE Plasma 6 Wayland,
 two 3840x2160 outputs at160% desktop scale, Unity6000.3.28f1, UI150%,
 render density2, Unity Hub3.22.5 installed from Flathub:
 menu titles open on the first click, dropdowns appear under the title instead of
 at the screen edge, no menu is mapped without being presented, and focus restore
 after a popup closes works. The same session exercised the Hub Flatpak launcher
 path (`flatpak-spawn --host`) end to end.
- Evidence for the three menu defects came from the bridge debug log
 (`UNITY_SCALE_BRIDGE_LOG`), `_NET_CLIENT_LIST`, and X-side window geometry and
 pixel captures: a mapped, fully painted popup window with no associated
 wl_surface role, a popup configured from the output origin, and a press that
 never produced a popup window.

## Bridge inherited from prototype-v5.2

-109 library regression tests passed, including popup repeated configure,
 window-list lifecycle, cursor, viewport, output and input protocol tests.
- Local interactive testing: CachyOS, KWin6.8.2 Wayland, NVIDIA RTX4080,
 Unity6000.3.27f2, 3840x2160 display at145% desktop scale.
-100-point ruler blocks measured150px at150% and160px at160%.
- User confirmed menu proportions, click input, orbit, Transform label dragging,
 cursor size/theme, and (after final repairs) small menus and dock/undock.
- One earlier prototype exit was not explained or reproduced in a27-minute traced
 follow-up; it is not being represented as a proven crash-free implementation.

## Standalone application

The standalone application was installed into the real user account. A disposable
project launched through the Editor-wide wrapper, compiled its Editor script,
reported pixelsPerPoint2, UI150%, cursor backing72, and one native managed window,
and exited0. The helper and private display were cleaned up afterward.

A real uninstall restored the original Editor's exact SHA256, inode and mode;
configuration remained intact. Reinstall succeeded. Integration tests also cover
argv forwarding, utility bypass, fallback, update conflicts and guarded restore.
Installer failure-injection tests cover transactional rollback and owned-file /
symlink / manifest validation.

Native EditorApplication.OpenProject was tested with a replacement process.
The replacement retained the private display, reported the same150% settings,
and completed normal shutdown. The runner waits for matching replacement Editor
processes before stopping its helper. Original-process teardown returned SIGSEGV
with !gAlreadyClosing / Error relaunching Unity / s_EditorUserBuildSettings assertions.
Control experiments reproduced the same original-process assertion with the
unwrapped executable on both the bridge and native host DISPLAY:0 (PPP1). This
rules out the wrapper filename and bridge as prerequisites for that test failure.
It does not establish that every project-switch workflow has been validated.
Replacement exit code is not available to the supervisor after native reparenting;
the metadata records that limitation instead of inventing an exit status.

GTK smoke and focused behavior tests use isolated settings/fixtures. Native
Wayland captures checked the actual app layout. Latest interface behavior is
explicit scan, expandable Editor rows, one numeric percentage control, automatic
saving, visible collapsed scale and pending-edit confirmation.

The suite now holds95 tests (the original88 plus seven dependency-installer,
 Hub-Flatpak launcher and runner unit tests). The complete set runs in CI under
 Xvfb; in the contributing environment the70 non-GUI tests
 (`test_install`, `test_core`, `test_integration`, `test_runner`) passed and the
 GUI tests were not re-run locally.
The GUI was also checked with the standalone non-presenting smoke command.
The final disabled-label simplification passed all6 focused GUI-state tests.
The generated native release was extracted into a fresh directory, installed to
a separate prefix, executed through its installed entry point, and uninstalled
successfully. The real installation and user settings remained independent.
Visible Unity-related menu entries were checked during the original app
validation. Repeat this check after changing desktop-entry branding.
Unit tests are not substitutes for multi-machine graphical validation. Fake Editor
fixtures test process and restore behavior without changing real installations.
The first public [GitHub Actions build](https://github.com/FardinHaque70/unity-editor-ui-scalar-for-linux/actions/runs/41332053150)
passed on Ubuntu24.04: the locked bridge build/tests,87 Python/GTK tests under
Xvfb, standalone GTK smoke, desktop-entry validation, package creation and artifact
upload all completed successfully. This is a clean build/GUI test result, not
physical KDE/GPU/mixed-monitor validation on Ubuntu.

## Release scope

Local helper ABI: glibc2.43. Rebuild on the target distribution, or use a verified
older-ABI CI artifact once available. The GTK interface needs GTK4.10/libadwaita1.4.
Broader compositor/GPU/multi-monitor and additional Unity-version testing remains
outstanding. Existing passing measurements belong to the described machine.

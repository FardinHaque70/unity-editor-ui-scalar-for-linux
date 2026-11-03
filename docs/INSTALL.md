# Installation and command line

Unity Editor UI Scalar for Linux is a user-local KDE Wayland beta for x86_64
Linux. It needs Python 3.10+, GTK 4.10+, libadwaita 1.4+, Xwayland 23.1+, and a compositor supporting
xdg-shell, viewporter and xdg-output. Native X11 sessions are outside this beta's
scope. Other desktops and mixed-DPI hardware need further testing.

## System dependencies

Install dependencies through your distribution package manager:

```sh
# Arch / CachyOS
sudo pacman -S python python-gobject gtk4 libadwaita xorg-xwayland libxcb xcb-util-cursor kscreen wayland-utils

# Ubuntu 24.04 / KDE neon
sudo apt install python3 python3-gi gir1.2-gtk-4.0 gir1.2-adw-1 xwayland libxcb-cursor0 libx11-6 kscreen wayland-utils

# Fedora KDE
sudo dnf install python3 python3-gobject gtk4 libadwaita xorg-x11-server-Xwayland xcb-util-cursor libX11 kscreen wayland-utils
```

The installer checks the GTK versions and native bridge libraries. Older versions
of these distributions may not provide the required GTK or libadwaita release.

**The current locally built native helper requires glibc 2.43.** Build locally
on older distributions. The prepared release workflow targets an older ABI;
its configuration is not evidence of a successful external CI run. Check the
[release and validation notes](VALIDATION.md) for the artifact you use.

## Automated install

`./install.sh --with-dependencies` runs the whole install for the distributions
listed above:

1. `packaging/install-deps.sh` probes the running system and installs only the
   packages that are missing (with `sudo`, after confirmation).
2. `packaging/build-bridge.sh` builds the native helper when
   `libexec/xwayland-satellite` is absent. A release that already contains the
   helper skips this step and installs runtime packages only.
3. The normal user-local install runs, including its GTK, bridge and `ldd` checks.

The dependency script can be used alone:

```sh
./packaging/install-deps.sh --check          # report what is missing, install nothing
./packaging/install-deps.sh --dry-run        # show the package manager command
./packaging/install-deps.sh --runtime-only   # skip Rust/compiler/xcb headers
./packaging/install-deps.sh --yes            # no confirmation prompt
```

It handles the four package managers used by the supported distributions:

| Package manager | Distributions |
|---|---|
| `zypper` | openSUSE Tumbleweed, openSUSE Leap 15.6+, SLES |
| `apt-get` | Debian 13+, Ubuntu 23.10+, Linux Mint, Pop!_OS, Kali, Zorin |
| `dnf` | Fedora 38+, RHEL 9+, Rocky, Alma, CentOS Stream |
| `pacman` | Arch, Manjaro, EndeavourOS, CachyOS |

Package names that differ between releases are resolved against the running
repositories (`kscreen6`/`kscreen5`, `xorg-x11-server-Xwayland`/`xwayland`,
`pkgconf-pkg-config`/`pkgconf`, …), so a Leap 15.6 or Fedora install picks the
names that release actually carries. Every package list is also derived from
probes: GTK and libadwaita versions, Xwayland, `kscreen-doctor`, `wayland-info`,
`libX11`/`libxcb-cursor`, Rust, a C compiler, `pkg-config` and the xcb headers.
When every probe already passes the script changes nothing.

It refuses to run inside a Flatpak sandbox where host packages are not visible,
and it re-checks the result afterwards: if the distribution is older than the
minimum documented above, it reports the missing versions instead of leaving a
half-working install. Run it from a host terminal.

## Build the bridge locally

Install stable Rust, a C linker, clang, pkg-config, libxcb and xcb-util-cursor
headers. For Ubuntu, the native build packages are:

```sh
sudo apt install build-essential clang pkg-config libxcb1-dev libxcb-cursor-dev
```

From the repository root:

```sh
./packaging/build-bridge.sh
./install.sh
```

The build script uses locked Rust dependencies and runs the bridge regression
suite. Complete modified source is in
`bridge/unity-scaling-xwayland-satellite-v5.2`. A compatible prebuilt release
does not require a Rust toolchain.

## Install

From an extracted release with a compatible helper, run:

```sh
./install.sh
```

No sudo or pip is needed for the application installation. The default prefix
is `~/.local`. Open **Unity Editor UI Scalar for Linux** from your application
menu, or run
`~/.local/bin/unity-scale`. Distribution GTK libraries are used; an AppImage is
not supplied.

The installer stages all payloads before replacing an existing installation and
checks ownership before an upgrade. See [release packaging](RELEASING.md) for
custom-prefix and release workflows.

### Why there is no Flatpak or AppImage

This app orchestrates host processes instead of running inside a closed runtime.
The supervisor must start the host Unity Editor, the helper must start the host
`Xwayland`, and both must share the private X11 socket the helper creates so that
Unity renders into the bridge display. A Flatpak sandbox gives the app a private
`/tmp`, its own libraries, and no permission to execute or replace host Editor
executables, which breaks both the display handoff and the Unity Hub wrapper.
`flatpak-spawn --host` cannot repair that: the X11 socket still has to be shared
between a sandboxed helper and an unsandboxed Editor. Exporting the helper to the
host instead leaves `Xwayland`, `xcb`, `kscreen` and the GTK stack to the host, so
it resolves no dependencies. The installer above installs exactly those packages.

## Command line

```sh
unity-scale editors
unity-scale configure --editor /path/to/Editor/Unity --scale 150
unity-scale enable --editor /path/to/Editor/Unity
unity-scale run --editor /path/to/Editor/Unity --project '/path/to/game'
unity-scale run --editor /path/to/Editor/Unity --dry-run -- -projectPath '/path/to/game'
unity-scale diagnostics
unity-scale disable --editor /path/to/Editor/Unity
```

Profiles use the installed Editor path as their identity. GUI and command-line
settings are shared. A dry run reports the launch plan without starting Unity.
See [Editor integration](INTEGRATION.md) before updating or moving an Editor.

Configuration is stored in `$XDG_CONFIG_HOME/unity-scale`, defaulting to
`~/.config/unity-scale`. Logs and integration state use
`$XDG_STATE_HOME/unity-scale`, defaulting to `~/.local/state/unity-scale`.
Diagnostics omit project paths and raw Editor logs. Raw runtime logs may contain
private information; they are not uploaded automatically.

The display bridge logs warnings only. Set `UNITY_SCALE_BRIDGE_LOG=debug` (or
`trace`) in the environment that launches the Editor, for example
`UNITY_SCALE_BRIDGE_LOG=debug unity-scale run --editor ...`, to record window,
popup and input geometry under the run's `bridge.log`.

## Uninstall

For the default installation:

```sh
~/.local/share/unity-scale/application/uninstall.sh
```

Uninstall restores this installation's enabled Editor wrappers before removing
the app. An ownership conflict or changed original backup stops removal rather
than overwriting a changed executable. Configuration and logs are retained for
troubleshooting. See [restoration details](INTEGRATION.md#disable-updates-and-removal).

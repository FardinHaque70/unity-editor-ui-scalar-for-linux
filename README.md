<img src="resources/branding/penguin-unity-v1.png" width="128" alt="Unity Editor UI Scalar for Linux icon">

# Unity Editor UI Scalar for Linux

Comfortable Unity Editor scaling on **KDE Wayland**.

On a high-resolution Linux display, Unity can be too small to use comfortably.
Common scaling workarounds can also leave menus, cursors, popups and docked
windows at inconsistent sizes. This app provides an Editor-wide percentage
control and a dedicated display bridge that addresses these problems together.

![Unity Editor UI Scalar for Linux settings window](docs/screenshots/main.png)

**0.1.0-beta.1 · KDE Wayland · x86_64**

## How it works

Each Editor runs through an isolated, rootless Xwayland bridge. Unity renders at
high density, and the Wayland compositor resamples it to the selected size.
The bridge normalizes GTK font and cursor sizing, preserves popup geometry, and
exposes the window information Unity needs for docking.

Settings apply to every project using that Editor. Enabling an override installs
a reversible launch wrapper at its existing executable path, so normal Unity Hub
launches use the setting. The original executable is preserved beside it; its
compiled bytes are never patched. No scripts, packages or settings are added to
projects, and desktop scaling stays unchanged.

## Install

On a fresh x86_64 KDE Wayland system, one command installs the distribution
packages, builds the bridge from source, and installs the app:

```sh
./install.sh --with-dependencies
```

It asks before installing packages with `sudo`. Run it from a host terminal, not
from inside a Flatpak or container terminal. To install packages yourself, or to
use a release that already contains the bridge, use the plain installer:

From an extracted release, or a checkout with the bridge built:

```sh
./install.sh
```

Installation is user-local. Open **Unity Editor UI Scalar for Linux** from your
application menu.

Requires Python 3.10+, GTK 4.10+, libadwaita 1.4+, Xwayland 23.1+, and an x86_64
KDE Wayland session. **The current native helper requires glibc 2.43.** On older
distributions, build the bridge locally before installation. See
[dependencies and source builds](docs/INSTALL.md).

## Use

1. Click **Scan for Installed Editors**, or **Add Editor** beside it.
2. Expand an Editor, enable **Override scale**, and enter a percentage such as 150.
   Valid changes save automatically.
3. **Close and reopen Unity through Unity Hub** to apply the setting.

Unity Hub may be installed natively or as the Flathub package; with the Flatpak,
the Editor, bridge and private display run on the host, started through
`flatpak-spawn --host`. See [integration and restoration](docs/INTEGRATION.md).

Running Editors do not change immediately. You can close the app while Unity
runs. **Turn off Override scale before updating or uninstalling an Editor in Hub.**
See [integration and restoration](docs/INTEGRATION.md).

## Beta status and documentation

Local validation includes 88 application tests, 109 bridge tests, physical scale
measurements, menus, cursor consistency, input and docking. Broader GPU,
mixed-DPI and compositor testing remains outstanding; see the
[validation record](docs/VALIDATION.md) and [architecture](docs/ARCHITECTURE.md).
[Command-line tools and uninstall instructions](docs/INSTALL.md#command-line)
cover additional workflows.

Independent and unofficial; Unity Editor and Hub are not included. Built on
[xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) by Supreeeme
and contributors. Application and modified bridge: [MPL 2.0](LICENSE).
See [NOTICE](NOTICE) for attribution and third-party licensing.

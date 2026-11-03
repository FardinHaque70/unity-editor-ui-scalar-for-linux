#!/usr/bin/env bash
# Install the distribution packages this app and its bridge need.
#
# This script is optional: it only saves you from looking up package names. It
# never builds or installs the app itself, and it changes nothing when every
# probe below already passes. See docs/INSTALL.md.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: packaging/install-deps.sh [options]

Installs runtime dependencies (GTK4, libadwaita, Xwayland, xcb, kscreen,
wayland-utils) and, unless a prebuilt bridge is present, the build
dependencies (Rust, a C compiler, clang, pkg-config, libxcb and
xcb-util-cursor headers).

Options:
  --check          Report what is missing and exit non-zero; install nothing.
  --runtime-only   Skip the bridge build dependencies.
  --build-only     Install only the bridge build dependencies.
  --dry-run        Print the package manager commands without running them.
  --yes, -y        Do not ask for confirmation before installing.
  -h, --help       Show this help.

Supported package managers: zypper (openSUSE/SLES), apt-get (Debian, Ubuntu,
Linux Mint, Pop!_OS, …), dnf (Fedora, RHEL, Rocky, Alma), pacman (Arch,
Manjaro, EndeavourOS, CachyOS). Unknown distributions get the package names
for their package manager, not for their release.

The script uses sudo when it is not already running as root.
EOF
}

mode=all
dry_run=0
assume_yes=0
check_only=0
for arg in "$@"; do
  case "$arg" in
    --runtime-only) mode=runtime ;;
    --build-only) mode=build ;;
    --dry-run) dry_run=1 ;;
    --check) check_only=1 ;;
    --yes|-y) assume_yes=1 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown option: %s\n\n' "$arg" >&2; usage >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
run() {
  if ((dry_run)); then printf '+ %s\n' "$*"; else "$@"; fi
}


if [[ -f /.flatpak-info || -n ${FLATPAK_ID:-} ]]; then
  say 'This terminal runs inside a Flatpak sandbox, so host packages cannot be inspected or installed.'
  say 'Run this script from a host terminal (for example Konsole) instead.'
  exit 2
fi

manager=
if command -v zypper >/dev/null 2>&1; then
  manager=zypper
elif command -v apt-get >/dev/null 2>&1; then
  manager=apt
elif command -v dnf >/dev/null 2>&1; then
  manager=dnf
elif command -v pacman >/dev/null 2>&1; then
  manager=pacman
else
  say 'No supported package manager (zypper, apt-get, dnf, pacman) was found.'
  say 'Install these packages manually, then run ./install.sh:'
  say '  Python 3.10+ with PyGObject, GTK 4.10+, libadwaita 1.4+, Xwayland 23.1+,'
  say '  libxcb, xcb-util-cursor, kscreen, wayland-utils'
  say '  For the bridge: Rust, a C compiler, clang, pkg-config, libxcb headers,'
  say '  xcb-util-cursor headers'
  exit 2
fi

distro=$( (. /etc/os-release 2>/dev/null && printf '%s' "${PRETTY_NAME:-${ID:-unknown}}") || echo unknown )

# Package name alternatives are written "first|second|third" and resolved
# against the repository of the running distribution.
runtime_packages=()
build_packages=()
case "$manager" in
  zypper)
    runtime_packages=(python3-gobject typelib-1_0-Gtk-4_0 typelib-1_0-Adw-1
      libgtk-4-1 libadwaita-1-0 xwayland libxcb1 libxcb-cursor0 libX11-6
      'kscreen6|kscreen5|kscreen' wayland-utils)
    build_packages=(rust cargo gcc clang pkgconf libxcb-devel xcb-util-cursor-devel)
    ;;
  apt)
    runtime_packages=(python3 'python3-gi' 'gir1.2-gtk-4.0' 'gir1.2-adw-1'
      xwayland libxcb1 libxcb-cursor0 libx11-6 'kscreen|kscreen5' wayland-utils)
    build_packages=(build-essential clang pkg-config libxcb1-dev libxcb-cursor-dev)
    ;;
  dnf)
    runtime_packages=(python3 python3-gobject gtk4 libadwaita
      'xorg-x11-server-Xwayland|xwayland' libxcb xcb-util-cursor libX11
      'kscreen|kscreen6|kscreen5' wayland-utils)
    build_packages=(rust cargo gcc clang 'pkgconf-pkg-config|pkgconf' libxcb-devel
      xcb-util-cursor-devel)
    ;;
  pacman)
    runtime_packages=(python python-gobject gtk4 libadwaita
      'xorg-xwayland|xwayland' libxcb xcb-util-cursor libx11 kscreen wayland-utils)
    build_packages=(rust gcc clang pkgconf libxcb xcb-util-cursor)
    ;;
esac

package_exists() {
  case "$manager" in
    zypper) zypper --non-interactive info "$1" >/dev/null 2>&1 ;;
    apt) apt-cache show "$1" >/dev/null 2>&1 ;;
    dnf) dnf -q info "$1" >/dev/null 2>&1 ;;
    pacman) pacman -Si "$1" >/dev/null 2>&1 ;;
  esac
}

# First name in "a|b|c" that this distribution actually carries.
resolve_package() {
  local candidate
  local alternatives=()
  IFS='|' read -r -a alternatives <<<"$1"
  for candidate in "${alternatives[@]}"; do
    if package_exists "$candidate"; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  printf '%s' "${alternatives[0]}"
}

python_bin=/usr/bin/python3
[[ -x $python_bin ]] || python_bin=$(command -v python3 || true)

runtime_missing=()
if [[ -z $python_bin ]]; then
  runtime_missing+=('python3')
elif ! "$python_bin" - <<'PY' >/dev/null 2>&1
import gi
gi.require_version('Gtk','4.0');gi.require_version('Adw','1')
from gi.repository import Gtk, Adw
assert (Gtk.get_major_version(),Gtk.get_minor_version()) >= (4,10)
assert (Adw.get_major_version(),Adw.get_minor_version()) >= (1,4)
PY
then
  runtime_missing+=('PyGObject with GTK 4.10+ and libadwaita 1.4+')
fi
for program in Xwayland kscreen-doctor wayland-info; do
  command -v "$program" >/dev/null 2>&1 || runtime_missing+=("$program")
done
if [[ -n $python_bin ]] && ! "$python_bin" - <<'PY' >/dev/null 2>&1
import ctypes.util
assert ctypes.util.find_library('X11'), 'libX11'
assert ctypes.util.find_library('xcb-cursor'), 'libxcb-cursor'
PY
then
  runtime_missing+=('libX11 and libxcb-cursor')
fi

build_missing=()
for program in rustc cargo pkg-config; do
  command -v "$program" >/dev/null 2>&1 || build_missing+=("$program")
done
compiler=
for program in cc gcc clang; do
  if command -v "$program" >/dev/null 2>&1; then compiler=$program; break; fi
done
[[ -n $compiler ]] || build_missing+=('a C compiler')
if command -v pkg-config >/dev/null 2>&1; then
  pkg-config --exists xcb 2>/dev/null || build_missing+=('libxcb headers')
  pkg-config --exists xcb-cursor 2>/dev/null || build_missing+=('xcb-util-cursor headers')
fi

want_runtime=0
want_build=0
[[ $mode != build && ${#runtime_missing[@]} -gt 0 ]] && want_runtime=1
[[ $mode != runtime && ${#build_missing[@]} -gt 0 ]] && want_build=1

report_status() {
  say "Distribution: $distro"
  say "Package manager: $manager"
  if [[ ${#runtime_missing[@]} -eq 0 ]]; then
    say '  runtime dependencies: satisfied'
  else
    say "  runtime dependencies: missing ${runtime_missing[*]}"
  fi
  if [[ $mode == runtime ]]; then
    return
  fi
  if [[ ${#build_missing[@]} -eq 0 ]]; then
    say '  bridge build dependencies: satisfied'
  else
    say "  bridge build dependencies: missing ${build_missing[*]}"
  fi
}

if ((check_only)); then
  report_status
  ((want_runtime + want_build == 0))
  exit $?
fi

if ((!want_runtime && !want_build)); then
  report_status
  say 'Nothing to install.'
  if [[ $mode != runtime && ! -x "$(dirname "${BASH_SOURCE[0]}")/../libexec/xwayland-satellite" ]]; then
    say 'Next: ./packaging/build-bridge.sh && ./install.sh'
  else
    say 'Next: ./install.sh'
  fi
  exit 0
fi

if [[ $EUID -eq 0 ]]; then
  sudo_cmd=()
elif command -v sudo >/dev/null 2>&1; then
  sudo_cmd=(sudo)
else
  sudo_cmd=()
  if ((!dry_run)); then
    say 'Root privileges are required. Re-run this script as root, or install the packages above manually.'
    exit 1
  fi
fi

# apt needs an updated index before package names can be resolved or installed.
if [[ $manager == apt ]] && ((!dry_run)); then
  run "${sudo_cmd[@]}" apt-get update
fi

selected=()
if ((want_runtime)); then
  for spec in "${runtime_packages[@]}"; do selected+=("$(resolve_package "$spec")"); done
fi
if ((want_build)); then
  for spec in "${build_packages[@]}"; do selected+=("$(resolve_package "$spec")"); done
fi

# Preserve declaration order while removing duplicate package names.
deduplicated=()
for package in "${selected[@]}"; do
  duplicate=0
  for existing in ${deduplicated[@]+"${deduplicated[@]}"}; do
    [[ $existing == "$package" ]] && duplicate=1 && break
  done
  ((duplicate)) || deduplicated+=("$package")
done

report_status
say 'Packages to install:'
printf '  %s\n' "${deduplicated[@]}"

install_command() {
  case "$manager" in
    zypper) printf '%s\n' "${sudo_cmd[*]:-}${sudo_cmd[*]:+ }zypper --non-interactive install ${deduplicated[*]}" ;;
    apt) printf '%s\n' "${sudo_cmd[*]:-}${sudo_cmd[*]:+ }apt-get install -y ${deduplicated[*]}" ;;
    dnf) printf '%s\n' "${sudo_cmd[*]:-}${sudo_cmd[*]:+ }dnf install -y ${deduplicated[*]}" ;;
    pacman) printf '%s\n' "${sudo_cmd[*]:-}${sudo_cmd[*]:+ }pacman -S --needed --noconfirm ${deduplicated[*]}" ;;
  esac
}

if ((dry_run)); then
  [[ $manager == apt ]] && printf '+ %s\n' "${sudo_cmd[*]:-}${sudo_cmd[*]:+ }apt-get update"
  printf '+ %s\n' "$(install_command)"
  exit 0
fi

if ((!assume_yes)) && [[ -t 0 ]]; then
  read -r -p 'Install these packages now? [y/N] ' reply || reply=
  case "$reply" in
    [Yy]|[Yy][Ee][Ss]) ;;
    *) say 'Cancelled. No packages were installed.'; exit 1 ;;
  esac
fi

if [[ ${#sudo_cmd[@]} -gt 0 ]]; then say 'sudo is needed to install system packages.'; fi

case "$manager" in
  zypper) run "${sudo_cmd[@]}" zypper --non-interactive install "${deduplicated[@]}" ;;
  apt) run "${sudo_cmd[@]}" apt-get install -y "${deduplicated[@]}" ;;
  dnf) run "${sudo_cmd[@]}" dnf install -y "${deduplicated[@]}" ;;
  pacman) run "${sudo_cmd[@]}" pacman -S --needed --noconfirm "${deduplicated[@]}" ;;
esac

say 'Dependencies installed.'
if [[ $mode != runtime && ! -x "$(dirname "${BASH_SOURCE[0]}")/../libexec/xwayland-satellite" ]]; then
  say 'Next: ./packaging/build-bridge.sh && ./install.sh'
else
  say 'Next: ./install.sh'
fi

# Verify the result: an old release cannot install a newer GTK, and a missing
# bridge runtime surfaces here rather than at the first scaled launch.
problems=()
if [[ -n $python_bin ]] && ! "$python_bin" - <<'PY' >/dev/null 2>&1
import gi
gi.require_version('Gtk','4.0');gi.require_version('Adw','1')
from gi.repository import Gtk, Adw
assert (Gtk.get_major_version(),Gtk.get_minor_version()) >= (4,10)
assert (Adw.get_major_version(),Adw.get_minor_version()) >= (1,4)
PY
then
  problems+=('GTK 4.10+ and libadwaita 1.4+ are required but this distribution provides older versions')
fi
for program in Xwayland kscreen-doctor wayland-info; do
  command -v "$program" >/dev/null 2>&1 || problems+=("$program is still missing")
done
for program in rustc cargo; do
  [[ $mode == runtime ]] && break
  command -v "$program" >/dev/null 2>&1 || problems+=("$program is still missing")
done
if [[ ${#problems[@]} -gt 0 ]]; then
  say ''
  say 'Some dependencies are still missing:'
  printf '  - %s\n' "${problems[@]}"
  say ''
  say 'The application needs Python 3.10+, GTK 4.10+, libadwaita 1.4+, Xwayland 23.1+'
  say 'and a KDE Wayland session. Distributions older than Ubuntu 23.10,'
  say 'Debian 13, Fedora 38 or openSUSE Leap 15.6 do not satisfy the GTK requirement.'
  exit 1
fi
say 'All dependency checks pass.'

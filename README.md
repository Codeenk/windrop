# WinDrop

Drop a Windows `.exe` on the window and it becomes an application in your menu.

WinDrop works out what the program needs, builds a private environment for it,
runs the installer, finds what was installed, writes a menu entry with the
program's own icon, and then gets out of the way. Removing it removes everything
it created. It never asks for root, and it never installs anything system-wide.

```
┌──────────────────────────────────────────┐
│                                          │
│      Drop a Windows program here         │
│                                          │
│      Or click Choose a file…             │
│                                          │
└──────────────────────────────────────────┘
```

## Why each program gets its own environment

The usual way a Wine problem is solved is by changing something global: a DLL
override here, a different Windows version there, another runtime in the shared
prefix. Every fix helps one program and quietly risks the next.

WinDrop gives each program its own Wine prefix, its own Windows version, its own
DLL overrides and its own copy of whatever runtime it needs. Shared pieces —
Wine itself, DXVK, VKD3D-Proton — are version-pinned once under WinDrop's own
data directory and reused, so ten applications do not mean ten copies of Wine.

Nothing is written outside:

| Where | What |
|---|---|
| `~/.local/share/windrop/apps/<id>/` | one application: its prefix, its icon, its `metadata.json` |
| `~/.local/share/windrop/runtime/` | shared, version-pinned Wine, DXVK, VKD3D-Proton |
| `~/.local/share/windrop/profiles.db` | compatibility profiles, local and learned |
| `~/.local/share/windrop/logs/` | one log per run |
| `~/.config/windrop/config.json` | your settings |
| `~/.local/share/applications/org.windrop.WinDrop.<id>.desktop` | its menu entry |

`--data-dir DIR` (or `WINDDROP_DATA_DIR`) moves *all* of it inside `DIR` and keeps
the menu entries alongside — so a WinDrop installation can live on a removable
disk, or in a directory you back up, and leave nothing behind when it goes.

## Installing

### Arch Linux and derivatives

```sh
git clone https://github.com/windrop/windrop
cd windrop
makepkg -si
```

A `PKGBUILD` is in `packaging/`.

### From source

```sh
cargo build --release
sudo install -Dm755 target/release/windrop     /usr/local/bin/windrop
sudo install -Dm755 target/release/windrop-gui /usr/local/bin/windrop-gui
sudo install -Dm644 data/org.windrop.WinDrop.desktop \
  /usr/local/share/applications/org.windrop.WinDrop.desktop
sudo install -Dm644 data/org.windrop.WinDrop.metainfo.xml \
  /usr/local/share/metainfo/org.windrop.WinDrop.metainfo.xml
sudo install -Dm644 data/icons/hicolor/scalable/apps/windrop.svg \
  /usr/local/share/icons/hicolor/scalable/apps/windrop.svg
```

Building needs GTK4 development files, `pkgconf` and a C compiler. Rust 1.88 or
later.

### Flatpak and AppImage

A Flatpak manifest is in `packaging/org.windrop.WinDrop.yml` and an AppImage
build script in `packaging/appimage.sh`. Neither bundles Wine: see
[What WinDrop needs](#what-windrop-needs).

## What WinDrop needs

WinDrop does not install system packages. It detects what is present and tells
you exactly what is missing and what to type. Run:

```sh
windrop doctor
```

| Tool | Needed for | Without it |
|---|---|---|
| `wine` | running anything at all | nothing works — WinDrop says so up front |
| `winetricks` | the VC++ and .NET runtimes | installers that need them fail the way they would on a bare Windows |
| `bubblewrap` | isolation | programs run with access to your home directory |
| `icoutils` | pulling the icon out of the `.exe` | a generic icon is used |
| `cabextract` | unpacking Microsoft cabinets | `winetricks` cannot fetch anything |
| `desktop-file-utils` | refreshing the menu | the entry appears after your next login |

On Arch:

```sh
sudo pacman -S --needed wine winetricks bubblewrap icoutils cabextract desktop-file-utils
```

## Using it

### The window

Drag an installer onto it, or use **Choose a file…**. WinDrop tells you what the
file is — 32- or 64-bit, an installer or a program, .NET or native — and asks
once whether to install it. Installed programs are listed with a Launch button
and, behind the menu on each row, their details, their log, a way to stop
everything they are running, and a way to remove them.

Settings hides the four decisions worth making: which Wine to prefer, whether to
use DXVK and VKD3D-Proton, whether to isolate programs with bubblewrap, and how
to trade off latency against correctness. Everything else is decided per
application, from its profile.

### The command line

Every one of these also works over SSH, and every one of them takes `--json`:

```sh
windrop install ~/Downloads/npp.8.6.4.Installer.x64.exe
windrop list
windrop launch notepadpp
windrop inspect some-setup.exe        # what is it, and how would WinDrop run it?
windrop logs notepadpp --follow
windrop remove notepadpp
windrop doctor
windrop profiles list                 # recipes it knows
windrop profiles seed                 # copy the bundled ones into the database
windrop update                        # newer recipes for what you have installed
windrop config set dxvk_settings.max_frame_rate 144
windrop gui
```

`windrop install --dry-run FILE` prints the whole plan — the environment, the
commands, the sandbox flags — without changing a thing. It is the fastest way to
see what WinDrop is about to do.

## How it works

```
        .exe
         │
         ▼
   ┌───────────┐   bitness, subsystem, imports, digest
   │ inspector │
   └─────┬─────┘
         ▼
   ┌───────────┐   local database → remote registry → a generic recipe
   │ profiles  │   (what worked is remembered, and preferred next time)
   └─────┬─────┘
         ▼
   ┌───────────┐   Wine build, Windows version, DLL overrides,
   │ build     │   DXVK/VKD3D-Proton, winetricks dependencies,
   │ an env    │   bubblewrap mounts — as one inspectable plan
   └─────┬─────┘
         ▼
   ┌───────────┐   try each variant until one installs and something
   │ fallback  │   launchable appears; remember the winner
   └─────┬─────┘
         ▼
   metadata.json · icon.png · menu entry · installed
```

Two details are worth knowing.

**Isolation is a mount namespace, not a chroot.** `bwrap` gives the program a
private `/` in which the only writable things are its own prefix, and `$HOME` is
replaced by an empty directory. Your documents, your SSH keys and your other
applications are not merely hidden from the program — they are not reachable from
inside it. `--no-sandbox` turns that off if something legitimate needs it.

The network is deliberately *not* withheld. A browser, a chat client, a game or
anything with an updater is broken without it, and it is broken in a way you
cannot see the cause of. What WinDrop isolates is what a program can reach on
disk; where it can connect is your network's business, not WinDrop's.

**Nothing is decided twice.** The exact environment that worked is written into
the application's `metadata.json` and replayed at launch. Changing a setting
changes only future installations.

## Documentation

- [`docs/profile-format.md`](docs/profile-format.md) — the compatibility profile
  format, and how to write one for a program you care about.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — building, testing, and how the profile
  pipeline works.
- [`CHANGELOG.md`](CHANGELOG.md) — what changed, and when.

## What WinDrop does not do

- **It does not run games that require kernel-level anti-cheat.** No userspace
  translation layer can, and promising otherwise would waste your evening.
- **It does not manage Steam games.** Steam has its own Proton; use it.
- **It does not package a Wine.** Your distribution's Wine, or the WineHQ
  Flatpak, is the one WinDrop drives. That is deliberate: a bundled Wine would be
  several hundred megabytes, immediately out of date, and impossible to fix
  without a WinDrop release.
- **It does not touch your system Wine prefix.** `~/.wine` is never read or
  written. If you have spent years curating it, WinDrop is not going to disturb
  it.

## Licence

GPL-3.0-or-later. See [`LICENSE`](LICENSE).

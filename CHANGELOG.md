# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the versions are
[semantic](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **First-run setup.** The window now checks Wine and its helpers on every
  launch — a loading state while the probes run — instead of waiting for you
  to open Diagnostics. When something is missing, an **Install missing
  pieces** button installs it all in one go through your distribution's
  package manager (Arch, Debian/Ubuntu, Fedora and openSUSE; one privilege
  prompt via `pkexec`), streaming the installer's output into the window and
  re-checking afterwards. Where one click cannot work (an unknown
  distribution, Flatpak, no `pkexec`), the banner says why and keeps the
  copy-paste command.
- **`windrop doctor --install [--yes]`** does the same from a terminal, and
  the copy-paste setup command now speaks `apt-get`, `dnf` and `zypper` on
  those distributions instead of Arch names with an apology.

## [0.5.0] — 2026-09-23

The first release.

### Added

- **Drag-and-drop installation.** Drop an `.exe`, `.msi` or `.bat` on the window
  and WinDrop works out what it needs, builds an environment for it, runs the
  installer, finds the program that was installed, and adds it to the menu with
  its own icon. `windrop install FILE` does the same thing from a terminal.
- **One environment per application.** Every program gets its own Wine prefix,
  its own Windows version, its own DLL overrides and its own Windows runtime
  dependencies. Shared pieces — Wine, DXVK, VKD3D-Proton — are version-pinned
  once and reused. Nothing is installed system-wide, and WinDrop never needs
  root.
- **A complete command line.** `install`, `list`, `launch`, `remove`, `inspect`,
  `doctor`, `logs`, `profiles`, `update`, `config`, `gui` and `version`, all with
  `--json` for scripting and `--dry-run` where a change would be made.
- **Compatibility profiles.** A program's requirements are described as an ordered
  list of environments to try, so a program that needs something unusual does not
  need a second code path. Profiles come from an installer's SHA-256, the local
  database, the community registry, or — failing all of those — from what the
  executable itself declares. The environment that worked is recorded and
  replayed at launch.
- **Sixteen curated recipes** for programs people actually install, seeded into
  the local database so they work with no network access.
- **Graphics translation.** DXVK for Direct3D 9/10/11 and VKD3D-Proton for
  Direct3D 12, downloaded on first use and pinned per version.
- **Isolation with bubblewrap.** A program's mount namespace contains its own
  prefix and nothing else of yours: no documents, no SSH keys, no other
  applications. Shared folders can be granted deliberately. Network access is
  kept, because a program without it is broken in ways nobody can diagnose.
  Inside the Flatpak build this layer is skipped, since Flatpak's own sandbox is
  already in force and bubblewrap cannot be nested.
- **A diagnostic that names the problem.** `windrop doctor` reports the host, the
  Wine it found, which translation layers are present, whether bubblewrap is
  available, and the exact command that installs whatever is missing. It exits
  non-zero when the machine is not ready, so a script can tell.
- **One-click removal** that verifies its own work: the prefix, the icon, the
  metadata and the menu entry go, and a test asserts that nothing is left.

### Notes

- Programs that require kernel-level anti-cheat do not work, and no userspace
  translation layer can make them. WinDrop does not pretend otherwise.
- Steam games are out of scope; Proton already handles them.
- Wine is not bundled. WinDrop drives the Wine you have.

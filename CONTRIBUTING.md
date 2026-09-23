# Contributing to WinDrop

Thank you for considering it. This document covers the two things that are
specific to this project — how to build and test it, and how to contribute a
compatibility profile — and stays short about the rest.

## Getting set up

```sh
git clone https://github.com/windrop/windrop
cd windrop
cargo build --workspace
cargo test --workspace
```

You will need:

- Rust 1.88 or later
- GTK4 development files, `pkgconf` and a C compiler (`gtk4-devel`, `libgtk-4-dev`,
  or `gtk4` + `pkgconf` on Arch)
- Optional, for the smoke tests: `wine`, `bubblewrap`, `desktop-file-utils`

You do **not** need Wine to run the test suite. That is deliberate, and it is the
most important design decision in this repository.

## Testing without Wine

Wine is a moving target that behaves differently on every distribution, and
making it a prerequisite for the test suite would mean most contributors could
not run it. So two things are true instead:

**Windows executables are fabricated.** `windrop_core::fixtures` writes real,
well-formed PE images — a console program, a 64-bit GUI application, a .NET
assembly, an installer that imports `d3d11.dll` and `vcruntime140.dll` — from
scratch. They are valid enough that `goblin` parses them, which is what makes the
inspector testable. There is also a deliberately malformed one, because the
inspector must survive a packed installer rather than fail on it.

**Wine is a shell script.** `crates/windrop-core/tests/end_to_end.rs` writes a
mock `wine`, `winetricks`, `wrestool` and `icotool` into a temporary directory,
puts them on `PATH`, and runs the entire pipeline — inspect, resolve, build the
environment, run the installer, locate the program, extract the icon, write the
menu entry, launch, remove. The mocks record what they were asked to do in a
`trace.log`, so the assertions are about *the command line WinDrop built*, which
is where the interesting bugs are.

The same approach is used from the outside in
`crates/windrop-cli/tests/cli.rs`, which drives the real `windrop` binary as a
subprocess.

```sh
cargo test -p windrop-core --test end_to_end      # the pipeline
cargo test -p windrop-cli  --test cli             # the command line
cargo test                                       # everything
```

If you are working on the compatibility layer, run the end-to-end tests with
output: the trace is printed on failure and it usually explains the bug outright.

```sh
cargo test -p windrop-core --test end_to_end -- --nocapture
```

## Trying it against a real installer

The one thing the mock cannot check is whether a real Windows installer actually
installs. For that, use a Wine on your own machine and a disposable home:

```sh
export WINDDROP_DATA_DIR=$(mktemp -d)/windrop
cargo run -p windrop-cli -- install ~/Downloads/some-setup.exe --dry-run
cargo run -p windrop-cli -- install ~/Downloads/some-setup.exe --profile notepadpp -v
cargo run -p windrop-cli -- doctor
```

`--dry-run` prints the whole plan — environment, sandbox flags, the exact
commands — without changing anything. When something goes wrong, `windrop doctor`
first, then the log:

```sh
cargo run -p windrop-cli -- logs <app> -n 200
```

## Where the code lives

| Crate | Contents |
|---|---|
| `windrop-core` | everything that matters: PE inspection, profiles, the profile database, the runtime manager, the environment builder, the sandbox, the fallback chain, the application manager, the doctor |
| `windrop-cli` | argument parsing, output formatting, exit codes |
| `windrop-gui` | a GTK4 window and a worker-thread channel; no logic of its own |

Both front-ends are thin. If you find yourself making a decision in
`windrop-cli` or `windrop-gui` that `windrop-core` could make instead, move it.

Two conventions are worth knowing before you change anything:

**Expensive decisions are pure functions.** `CommandSpec`, `AppProfile`,
`RuntimeEnv`, `SandboxRequest` and the environment builder all produce data rather
than side effects, and the tests assert on that data. Almost every hard part of
this project is testable because of it — please keep it that way.

**Nothing is installed system-wide, and nothing needs root.** If a change would
require either, it needs a design discussion before it needs code.

## Contributing a compatibility profile

A profile is a recipe for one program. Contributions here are as valuable as code
changes: they are the difference between "this might work" and "this works".

The full format is documented in [`docs/profile-format.md`](docs/profile-format.md).
The short version:

1. Install the program and let WinDrop do its thing.
   ```sh
   windrop install ~/Downloads/SomeProgram-Setup.exe --name SomeProgram
   ```
2. See what it settled on: `windrop profiles show someprogram`.
3. Pin the recipe to the installer you used, so it applies to that exact file:
   ```sh
   windrop profiles attach someprogram ~/Downloads/SomeProgram-Setup.exe
   ```
4. Export it into `registry/submitted/`:
   ```sh
   windrop profiles export registry/submitted/someprogram.json --only someprogram
   ```
5. Open a pull request.

**Do not set `source`.** WinDrop fills it in; a hand-written value is misleading.

### What continuous integration checks

The `profiles` job runs `windrop profiles verify` over `registry/profiles.json`
and every file in `registry/submitted/`. It uses the same code path the
application uses at import time, so a file that passes here will load everywhere.
A file that fails gets the exact field that is wrong, as a PR comment.

### What reviewers look for

- **A pinned digest.** A recipe without one is a guess about the future. The
  exception is a program whose installer you cannot pin (a rolling web
  installer), and that is worth a sentence in `notes`.
- **A `main_exe_hint`** when the installer drops more than one executable, so the
  wrong one does not end up in the menu.
- **Variants that change one thing at a time.** A second variant that changes
  three variables teaches nobody anything when it works.
- **`notes` that say why**, not what.

A recipe in the repository is used by everyone who has not turned the registry
off. One that silently downgrades a program's Windows version for no stated
reason is worse than no recipe at all.

## Style

- `cargo fmt` and `cargo clippy -- -D warnings` are enforced by CI. Run them
  before pushing.
- Comments explain *why*. The code says what it does; a comment that repeats it
  is noise, and a comment that explains a decision is worth its weight.
- Tests assert on behaviour that a user would notice. A test that restates the
  implementation is a change-detector, not a test.
- No new dependency without a reason in the pull request. The dependency tree is
  a liability, and this project is small enough that most things can be written
  rather than pulled in.

## Packaging

- `packaging/PKGBUILD` — Arch. `makepkg -si` in a clean chroot before submitting.
- `packaging/org.windrop.WinDrop.yml` — Flatpak. Note that it deliberately skips
  WinDrop's own sandbox layer, because Flatpak's is already in force and
  bubblewrap cannot be nested.
- `packaging/appimage.sh` — AppImage.

None of them bundle Wine. If you think they should, please open an issue first:
it is a deliberate choice, and the reasoning is in the README.

## Releases

Releases are cut from a tag by `.github/workflows/release.yml`, which runs the
full test suite, builds the AppImage, produces the AUR tarball with a real
checksum, and publishes the whole thing. Add an entry to `CHANGELOG.md` in the
same pull request as the change — the release notes are taken from it.

## Reporting a problem

`windrop doctor` output is worth more than a description of the symptom. It
records the distribution, kernel, session type, which Wine was found, which of
DXVK and VKD3D-Proton are installed, whether bubblewrap is available, and what
each tool resolved to. Please paste it.

For a program that does not work, the profile matters too:

```sh
windrop profiles show <the profile it picked>
windrop logs <app> -n 200
```

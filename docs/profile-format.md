# The WinDrop profile format

A profile is a recipe: what a particular Windows program needs in order to work,
written down so that nobody has to work it out twice.

Profiles are JSON. One file may hold one profile or an array of them, which is
how the bundled recipes are stored — `registry/profiles.json` is an array.

```json
{
  "id": "notepadpp",
  "name": "Notepad++",
  "version": "8.6",
  "hashes": ["3f0c6a…"],
  "arch": "x86_64",
  "requirements": {
    "graphics": null,
    "dotnet": false,
    "notes": ["Installer is an NSIS bundle that unpacks a 64-bit program"]
  },
  "variants": [
    {
      "wine_build": "stable",
      "arch": "x86_64",
      "windows_version": "win10",
      "dxvk": false,
      "vkd3d_proton": false,
      "dll_overrides": [],
      "env": [],
      "dependencies": [],
      "rationale": "the default environment works"
    }
  ],
  "source": "bundled",
  "installer_args": ["/S"],
  "main_exe_hint": "Program Files/Notepad++/notepad++.exe",
  "notes": "Installs to Program Files; the installer offers to create a shortcut.",
  "updated_at": "2026-09-23T09:38:00Z"
}
```

## Top level

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | **yes** | Stable slug and database key. Lowercase letters, digits and `-`. |
| `name` | string | **yes** | Display name, shown in lists and used to name the installed application. |
| `version` | string | no | The application version this recipe was written against. |
| `hashes` | array of strings | no | SHA-256 digests of installers this recipe is known to apply to. |
| `arch` | `"x86"` \| `"x86_64"` \| `"arm64"` | no | Set when the recipe only applies to one architecture. |
| `requirements` | object | no | What the inspector concluded, for the record. |
| `variants` | array | **yes** | Environments to try, best first. At least one. |
| `source` | `"bundled"` \| `"local"` \| `"remote"` \| `"generated"` | no | Set by WinDrop. Do not write it by hand when contributing. |
| `installer_args` | array of strings | no | Flags that make the installer non-interactive (see below). |
| `main_exe_hint` | string | no | Where the program usually ends up, relative to the prefix's `drive_c`. |
| `notes` | string | no | Anything a maintainer should know. Shown in the window. |
| `updated_at` | string | no | ISO-8601 timestamp. |

### `requirements`

| Field | Type | Meaning |
|---|---|---|
| `graphics` | `"none"` \| `"d3d9"` \| `"d3d11"` \| `"d3d12"` \| `"vulkan"` \| `null` | Which 3D API the program uses. Decides whether DXVK or VKD3D-Proton helps. |
| `dotnet` | bool | The program is a managed assembly and needs a .NET runtime. |
| `notes` | array of strings | Short justifications, shown to the user. |

## Variants, and why there is more than one

Each entry in `variants` is a complete environment. WinDrop tries them in order,
stopping at the first one that installs successfully *and* leaves something
launchable behind. The one that worked is recorded with the application, so it is
replayed at launch rather than guessed at again.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `wine_build` | string | **yes** | `"stable"`, `"staging"`, `"system"`, or the name of a specific build. |
| `arch` | `"x86"` \| `"x86_64"` | **yes** | Architecture of the prefix. Must match the program. |
| `windows_version` | `"winxp"` \| `"win7"` \| `"win10"` \| `"win11"` | **yes** | What Windows the program is told it is running on. |
| `dxvk` | bool | **yes** | Translate Direct3D 9/10/11 to Vulkan. |
| `vkd3d_proton` | bool | **yes** | Translate Direct3D 12 to Vulkan. |
| `dll_overrides` | array of strings | no | `WINEDLLOVERRIDES` entries, e.g. `"d3d12=n,b"`. |
| `env` | array of `[name, value]` pairs | no | Extra environment variables for the Wine process. |
| `dependencies` | array of objects | no | `winetricks` verbs to install before the installer runs. |
| `rationale` | string | no | One line explaining this variant's strategy. Shown in the UI. |

A dependency is `{"verb": "vcrun2022", "reason": "the program imports the MSVC
2022 runtime", "optional": false}`. `optional` dependencies are attempted but
never fail an installation — useful for things that only help.

Order the variants from most likely to work to least. A good second variant
changes *one* thing: a different Wine build, or `winxp` instead of `win10`.

## Writing one

1. Install the program you care about, and let WinDrop do whatever it does.

   ```sh
   windrop install ~/Downloads/SomeProgram-Setup.exe --name SomeProgram
   ```

2. Look at what it settled on.

   ```sh
   windrop profiles list
   windrop profiles show someprogram
   ```

3. Record the installer's digest against the recipe. This is what makes the
   recipe apply to that exact file the next time somebody drops it on WinDrop.

   ```sh
   windrop profiles attach someprogram ~/Downloads/SomeProgram-Setup.exe
   ```

4. Improve the recipe by hand: add a `main_exe_hint`, an `installer_args`, an
   alternate variant that you know works, and a `notes` line that says why.

5. Check it, then export it.

   ```sh
   windrop profiles verify my-recipe.json
   windrop profiles export my-recipe.json --only someprogram
   ```

6. Open a pull request that adds the file to `registry/submitted/`. Continuous
   integration runs `windrop profiles verify` over every file there — the same
   code the application itself uses, so a file that passes here will load
   everywhere.

To test a recipe without waiting for a merge:

```sh
windrop profiles import my-recipe.json
windrop install ~/Downloads/SomeProgram-Setup.exe --profile someprogram
```

## Non-interactive installers

`installer_args` is only used when the user asks for a silent install. If the
flags are wrong the installer may do something unexpected, so they are treated as
a convenience rather than the default — an installer with a window is the default
because most of them need at least one answer from a human.

Common ones:

| Installer | Flags |
|---|---|
| NSIS | `/S` |
| Inno Setup | `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART` |
| MSI | `/qn /norestart` |
| InstallShield | `/s /v"/qn"` |

## Rules the validator enforces

- `id` and `name` are non-empty.
- `variants` has at least one entry.
- No variant uses `arm64`: ARM64 prefixes are not supported yet.

Anything else is accepted, including a profile whose first variant is unlikely to
work — the fallback chain exists precisely because guesses are sometimes wrong.

## Where profiles come from

WinDrop looks for a recipe in this order, and stops at the first hit:

1. **A profile pinned to the file's digest.** If the SHA-256 of the installer you
   dropped is in someone's `hashes`, that recipe is exact and is used as-is.
2. **The local database**, by the program's name — including anything learned on
   this machine, or shipped with WinDrop and seeded into the database.
3. **The community registry**, if network access is allowed.
4. **A generated profile**, built from what the inspector read out of the
   executable itself: its architecture, whether it is .NET, and which DLLs it
   imports. This is a real profile, not a fallback guess — it just has less
   knowledge than a human does.

Nothing is downloaded or sent anywhere else, and `--offline` turns step 3 off
entirely.

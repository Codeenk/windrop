//! Compatibility profiles: what an application needs in order to run.
//!
//! An [`AppProfile`] carries an *ordered* list of [`RuntimeEnv`] variants. The
//! first variant that installs cleanly and yields a launchable executable wins;
//! see [`crate::fallback`].
//!
//! Profiles come from four places, in decreasing order of trust:
//!
//! | Source                | Origin                                            |
//! |-----------------------|---------------------------------------------------|
//! | [`ProfileSource::Bundled`] | Shipped with WinDrop, seeded from the project |
//! | [`ProfileSource::Local`]   | Learned or edited on this machine             |
//! | [`ProfileSource::Remote`]  | Fetched from the community registry           |
//! | [`ProfileSource::Generated`] | Inferred from the executable itself         |
//!
//! Even when the last case applies the user gets a working install, because the
//! generated profile is built from real facts read out of the PE image.

use serde::{Deserialize, Serialize};

use crate::compat::pe::{Arch, PeInspection};
use crate::config::{Config, WineVariant};
use crate::{Error, Result};

/// Which 3D API an application talks to. Decides whether DXVK or
/// VKD3D-Proton is worth installing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GraphicsApi {
    #[serde(rename = "none")]
    None,
    // Named explicitly rather than by a blanket rename rule: `kebab-case` would
    // spell these `d3-d9`, and a profile format is read and written by hand.
    #[serde(rename = "d3d9")]
    D3D9,
    #[serde(rename = "d3d11")]
    D3D11,
    #[serde(rename = "vulkan")]
    Vulkan,
    #[serde(rename = "d3d12")]
    D3D12,
}

impl GraphicsApi {
    pub fn label(self) -> &'static str {
        match self {
            GraphicsApi::None => "no Direct3D",
            GraphicsApi::D3D9 => "Direct3D 9",
            GraphicsApi::D3D11 => "Direct3D 10/11",
            GraphicsApi::D3D12 => "Direct3D 12",
            GraphicsApi::Vulkan => "Vulkan",
        }
    }

    /// DXVK translates Direct3D 9, 10 and 11 to Vulkan.
    pub fn uses_dxvk(self) -> bool {
        matches!(self, GraphicsApi::D3D9 | GraphicsApi::D3D11)
    }

    /// VKD3D-Proton translates Direct3D 12 to Vulkan.
    pub fn uses_vkd3d(self) -> bool {
        matches!(self, GraphicsApi::D3D12)
    }
}

/// The Windows version to present to the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum WindowsVersion {
    WinXp,
    Win7,
    #[default]
    Win10,
    Win11,
}

impl WindowsVersion {
    /// The `winetricks` verb that switches a prefix to this version.
    pub fn winetricks_verb(self) -> &'static str {
        match self {
            WindowsVersion::WinXp => "winxp",
            WindowsVersion::Win7 => "win7",
            WindowsVersion::Win10 => "win10",
            WindowsVersion::Win11 => "win11",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            WindowsVersion::WinXp => "Windows XP",
            WindowsVersion::Win7 => "Windows 7",
            WindowsVersion::Win10 => "Windows 10",
            WindowsVersion::Win11 => "Windows 11",
        }
    }
}

/// One installable runtime component, expressed as a `winetricks` verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencySpec {
    /// The `winetricks` verb, e.g. `vcrun2022` or `dotnet48`.
    pub verb: String,
    /// Why it is needed, shown in logs and in the UI.
    #[serde(default)]
    pub reason: String,
    /// Optional dependencies are attempted but never fail an install.
    #[serde(default)]
    pub optional: bool,
}

impl DependencySpec {
    pub fn new(verb: &str, reason: &str) -> Self {
        DependencySpec {
            verb: verb.to_string(),
            reason: reason.to_string(),
            optional: false,
        }
    }

    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// A stable de-duplication key for the `winetricks` invocation.
    pub fn verb_key(&self) -> String {
        self.verb.trim().to_ascii_lowercase()
    }
}

/// What the inspector inferred the application needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Requirements {
    pub graphics: Option<GraphicsApi>,
    /// The application is a managed assembly and needs a .NET runtime.
    pub dotnet: bool,
    /// Short human-readable justifications, surfaced in the UI.
    pub notes: Vec<String>,
}

impl Requirements {
    pub fn graphics_api(&self) -> GraphicsApi {
        self.graphics.unwrap_or(GraphicsApi::None)
    }
}

/// One concrete environment to try.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEnv {
    /// Which Wine build to ask the runtime manager for.
    pub wine_build: String,
    /// Architecture of the prefix to create.
    pub arch: Arch,
    pub windows_version: WindowsVersion,
    pub dxvk: bool,
    pub vkd3d_proton: bool,
    /// `WINEDLLOVERRIDES` entries, e.g. `d3d12=n,b`.
    #[serde(default)]
    pub dll_overrides: Vec<String>,
    /// Extra environment variables for the Wine process.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Components to install into the prefix before running the installer.
    #[serde(default)]
    pub dependencies: Vec<DependencySpec>,
    /// A short label explaining this variant's strategy.
    #[serde(default)]
    pub rationale: String,
}

impl RuntimeEnv {
    /// `WINEDLLOVERRIDES` value, or `None` when nothing needs overriding.
    pub fn dll_overrides_value(&self) -> Option<String> {
        if self.dll_overrides.is_empty() {
            None
        } else {
            Some(self.dll_overrides.join(";"))
        }
    }

    /// A stable identity for a variant.
    ///
    /// Two variants with the same signature are the same environment, so the
    /// chain can de-duplicate them and a learned preference can be matched back
    /// to the right one. Every field that changes how the application runs a
    /// part of it; `rationale` deliberately does not, because it is prose
    /// written for a person rather than a difference in behaviour.
    pub fn signature(&self) -> String {
        let mut env: Vec<String> = self.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        // Sort so that the same pairs in a different order are one signature.
        env.sort();
        let mut deps: Vec<String> = self.dependencies.iter().map(|d| d.verb_key()).collect();
        deps.sort();
        let mut overrides = self.dll_overrides.clone();
        overrides.sort();

        format!(
            "build={}|arch={:?}|windows={:?}|dxvk={}|vkd3d={}|deps={}|overrides={}|env={}",
            self.wine_build,
            self.arch,
            self.windows_version,
            self.dxvk,
            self.vkd3d_proton,
            deps.join(","),
            overrides.join(","),
            env.join(",")
        )
    }

    /// Which Wine build this variant wants, as a [`WineVariant`].
    pub fn wine_variant(&self) -> WineVariant {
        match self.wine_build.as_str() {
            "staging" => WineVariant::Staging,
            "system" => WineVariant::System,
            "stable" | "" => WineVariant::Stable,
            other => WineVariant::Build(other.to_string()),
        }
    }
}

/// Where a profile came from.
///
/// `Local` is the deserialisation default: a document that does not state its
/// provenance is treated as a hand-made profile rather than something WinDrop
/// claims to have generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileSource {
    Bundled,
    #[default]
    Local,
    Remote,
    Generated,
}

impl ProfileSource {
    /// A short phrase naming this source, for a table column or a key-value
    /// line. It has to read sensibly on its own: `Local` covers both a recipe
    /// learned from a successful install and one written by hand. To name the
    /// item rather than describe it, use [`ProfileSource::phrase`].
    pub fn label(self) -> &'static str {
        match self {
            ProfileSource::Bundled => "bundled with WinDrop",
            ProfileSource::Local => "local to this machine",
            ProfileSource::Remote => "from the community registry",
            ProfileSource::Generated => "detected automatically",
        }
    }

    /// The same source as the object of a sentence: "matched …".
    pub fn phrase(self) -> &'static str {
        match self {
            ProfileSource::Bundled => "a recipe bundled with WinDrop",
            ProfileSource::Local => "a profile local to this machine",
            ProfileSource::Remote => "a profile in the community registry",
            ProfileSource::Generated => "a generated profile",
        }
    }
}

/// A complete compatibility recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppProfile {
    /// Stable slug, also used as the database key.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// SHA-256 digests of installers this profile is known to apply to.
    #[serde(default)]
    pub hashes: Vec<String>,
    /// Architecture the profile is written for, when it is arch-specific.
    #[serde(default)]
    pub arch: Option<Arch>,
    #[serde(default)]
    pub requirements: Requirements,
    /// Ordered variants; the first successful one is remembered.
    pub variants: Vec<RuntimeEnv>,
    #[serde(default)]
    pub source: ProfileSource,
    /// Command-line flags that make the installer non-interactive.
    #[serde(default)]
    pub installer_args: Vec<String>,
    /// Where the real program usually ends up, relative to the prefix.
    #[serde(default)]
    pub main_exe_hint: Option<String>,
    #[serde(default)]
    pub notes: String,
    /// ISO-8601 timestamp of the last update, when known.
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl AppProfile {
    /// True when this profile claims to handle a specific installer digest.
    pub fn matches_hash(&self, sha256: &str) -> bool {
        let want = sha256.to_ascii_lowercase();
        self.hashes.iter().any(|h| h.eq_ignore_ascii_case(&want))
    }

    /// The first variant, which is the preferred strategy.
    pub fn primary_variant(&self) -> Result<&RuntimeEnv> {
        self.variants
            .first()
            .ok_or_else(|| Error::ProfileNotFound(self.id.clone()))
    }

    /// Reject profiles that could not be executed.
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::Config {
                field: "profile.id".into(),
                reason: "must not be empty".into(),
            });
        }
        if self.name.trim().is_empty() {
            return Err(Error::Config {
                field: format!("profile '{}.name'", self.id),
                reason: "must not be empty".into(),
            });
        }
        if self.variants.is_empty() {
            return Err(Error::Config {
                field: format!("profile '{}.variants'", self.id),
                reason: "a profile needs at least one runtime variant".into(),
            });
        }
        for v in &self.variants {
            if v.arch == Arch::Arm64 {
                return Err(Error::Config {
                    field: format!("profile '{}.variants.arch'", self.id),
                    reason: "ARM64 prefixes are not supported yet".into(),
                });
            }
        }
        Ok(())
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(text: &str) -> Result<Self> {
        let p: AppProfile = serde_json::from_str(text)?;
        p.validate()?;
        Ok(p)
    }
}

/// Turn a display name into a filesystem- and URL-safe slug.
///
/// Non-ASCII characters are dropped rather than transliterated, which is
/// predictable and avoids two different names colliding after transliteration.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // suppresses leading dashes
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "app".to_string()
    } else {
        // Keep filenames reasonable and definitely below filesystem limits.
        out.chars().take(64).collect()
    }
}

/// Map imported DLLs onto `winetricks` verbs, in a deterministic order.
///
/// The list is ordered from "most likely to be the blocker" to "nice to have",
/// because a failed `winetricks` invocation aborts the variant.
pub fn infer_dependencies(inspection: &PeInspection) -> Vec<DependencySpec> {
    let mut deps: Vec<DependencySpec> = Vec::new();
    let mut push = |dep: DependencySpec| {
        let key = dep.verb_key();
        if let Some(existing) = deps.iter_mut().find(|d| d.verb_key() == key) {
            // A required entry always beats an optional one.
            if existing.optional && !dep.optional {
                *existing = dep;
            }
        } else {
            deps.push(dep);
        }
    };

    if inspection.dotnet {
        push(DependencySpec::new(
            "dotnet48",
            "the application is a .NET assembly",
        ));
    }

    // Visual C++ runtimes. Only the newest matching generation is installed:
    // VC++ 2022 is side-by-side compatible with 2015-2019 binaries.
    const VCRUN: &[(&str, &str)] = &[
        ("vcruntime140", "vcrun2022"),
        ("msvcp140", "vcrun2022"),
        ("vcruntime140_1", "vcrun2022"),
        ("msvcr120", "vcrun2013"),
        ("msvcr110", "vcrun2012"),
        ("msvcr100", "vcrun2010"),
        ("msvcr90", "vcrun2008"),
        ("msvcr80", "vcrun2005"),
    ];
    for (dll, verb) in VCRUN {
        if inspection.imports_dll(dll) {
            push(DependencySpec::new(
                verb,
                &format!("{dll}.dll is required by the application"),
            ));
        }
    }

    // DirectX helper libraries and the D3D compilers that ship with them.
    const D3DEX: &[(&str, &str)] = &[
        ("d3dx9_43", "d3dx9"),
        ("d3dx9_42", "d3dx9"),
        ("d3dx9_41", "d3dx9"),
        ("d3dx9_36", "d3dx9"),
        ("d3dx9_31", "d3dx9"),
        ("d3dx9_30", "d3dx9"),
        ("d3dx9_27", "d3dx9"),
        ("d3dx9_26", "d3dx9"),
        ("d3dx11_43", "d3dx11"),
        ("d3dcompiler_43", "d3dcompiler_43"),
        ("d3dcompiler_47", "d3dcompiler_47"),
        ("xinput1_3", "xinput"),
        ("xinput9_1_0", "xinput"),
    ];
    for (dll, verb) in D3DEX {
        if inspection.imports_dll(dll) {
            push(DependencySpec::new(
                verb,
                &format!("{dll}.dll is required by the application"),
            ));
        }
    }

    // XML and rich-text components are common in installers.
    if inspection.imports_dll("msxml3") || inspection.imports_dll("msxml6") {
        push(DependencySpec::new("msxml6", "XML services are required"));
    }
    if inspection.imports_dll("riched20") || inspection.imports_dll("riched32") {
        push(DependencySpec::new(
            "riched20",
            "the rich-edit control is required",
        ));
    }
    if inspection.imports_dll("mfc140") || inspection.imports_dll("mfc120") {
        push(DependencySpec::new("mfc140", "the MFC runtime is required"));
    }

    // Fonts are almost always wanted by GUI applications, but they are a large
    // download and many applications render fine without them.
    if inspection.gui && !inspection.is_dll {
        push(
            DependencySpec::new("corefonts", "standard Windows fonts for a GUI application")
                .optional(),
        );
    }

    deps
}

/// Infer the graphics API and other requirements from imports.
pub fn infer_requirements(inspection: &PeInspection) -> Requirements {
    let mut notes = Vec::new();
    let graphics = if inspection.imports_dll("d3d12") {
        notes.push("Direct3D 12 imports found; VKD3D-Proton will be used".to_string());
        Some(GraphicsApi::D3D12)
    } else if inspection.imports_dll("d3d11")
        || inspection.imports_dll("d3d10core")
        || inspection.imports_dll("d3d10")
    {
        notes.push("Direct3D 10/11 imports found; DXVK will be used".to_string());
        Some(GraphicsApi::D3D11)
    } else if inspection.imports_dll("d3d9")
        || inspection.imports_dll("d3d8")
        || inspection.imports_dll("ddraw")
    {
        notes.push("Direct3D 9 imports found; DXVK will be used".to_string());
        Some(GraphicsApi::D3D9)
    } else if inspection.imports_dll("vulkan-1") {
        notes.push("the application uses Vulkan directly".to_string());
        Some(GraphicsApi::Vulkan)
    } else {
        None
    };

    if inspection.dotnet {
        notes.push("managed (.NET) assembly detected".to_string());
    }
    if !inspection.imports_readable {
        notes.push(
            "the import table could not be read (possibly packed); a generic profile was built"
                .to_string(),
        );
    }

    Requirements {
        graphics,
        dotnet: inspection.dotnet,
        notes,
    }
}

/// The Windows version an application is most likely to expect.
///
/// The linker timestamp is a useful hint: software built for Windows 9x/XP era
/// toolchains often refuses to start on a modern prefix.
pub fn guess_windows_version(inspection: &PeInspection) -> WindowsVersion {
    // A hard dependency beats a heuristic hint: the .NET 4.8 installer simply
    // refuses to run on a prefix claiming to be Windows XP, so a managed
    // assembly must never land on XP even if its linker timestamp is ancient.
    if inspection.dotnet {
        return WindowsVersion::Win7;
    }
    // 2005-06-01 as a Unix timestamp: a rough "predates Windows Vista" line.
    const YEAR_2005: u32 = 1_118_102_400;
    if inspection.timestamp != 0 && inspection.timestamp < YEAR_2005 {
        WindowsVersion::WinXp
    } else {
        WindowsVersion::Win10
    }
}

/// Words that distributors add to installer filenames and that say nothing
/// about the application itself.
const INSTALLER_NOISE: &[&str] = &[
    "setup",
    "setupx64",
    "setupx86",
    "install",
    "installer",
    "installerx64",
    "x64",
    "x86",
    "win32",
    "win64",
    "amd64",
];

/// Split an identifier into words at separators, camel-case boundaries and
/// letter/digit boundaries.
///
/// `"NotepadSetup"` becomes `["Notepad", "Setup"]` and `"vcredist_x86"`
/// becomes `["vcredist", "x86"]`, which is what makes noise removal work on
/// names that were never separated in the first place.
pub fn split_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut prev_was_lower = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            // "NotepadSetup" -> split before the capital S.
            if ch.is_ascii_uppercase() && prev_was_lower && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            // "x86" stays one word, but "dx9ex" -> "dx9", "ex".
            if ch.is_ascii_alphabetic() && current.ends_with(|c: char| c.is_ascii_digit()) {
                words.push(std::mem::take(&mut current));
            }
            current.push(ch);
            prev_was_lower = ch.is_ascii_lowercase();
        } else {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev_was_lower = false;
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn is_installer_noise(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    INSTALLER_NOISE.contains(&lower.as_str())
}

/// Turn an installer filename stem into something a person would recognise as
/// the application's name.
///
/// Falls back to the original stem when every word was noise, so `setup.exe`
/// is still called `setup` rather than becoming empty.
pub fn clean_app_name(stem: &str) -> String {
    let words: Vec<String> = split_words(stem);
    let kept: Vec<&str> = words
        .iter()
        .map(|w| w.as_str())
        .filter(|w| !is_installer_noise(w))
        .collect();
    if kept.is_empty() {
        stem.trim().to_string()
    } else {
        kept.join(" ")
    }
}

/// A slug used to match an installer against a registry profile by name.
///
/// Registry profiles are keyed by digest in the best case, but plenty of
/// applications ship new installers constantly. The slug lets a profile named
/// `Notepad++` be recognised from `npp.8.6.2.Setup.exe`.
pub fn name_hint_from_stem(stem: &str) -> String {
    slugify(&clean_app_name(stem))
}

/// Build the ordered list of variants to attempt.
///
/// The chain is deliberately short: each step costs a full prefix creation and
/// installer run. Every step differs in exactly one dimension from the previous
/// one, so a success or failure is informative.
pub fn build_variants(
    inspection: &PeInspection,
    requirements: &Requirements,
    config: &Config,
) -> Vec<RuntimeEnv> {
    build_variants_from(
        VariantFacts {
            arch: inspection.arch,
            graphics: requirements.graphics_api(),
            dependencies: infer_dependencies(inspection),
            windows_version: guess_windows_version(inspection),
        },
        config,
    )
}

/// Everything [`build_variants`] needs, without requiring a PE inspection.
///
/// `.msi` packages and `.bat` scripts are not PE images, so there is nothing to
/// infer from; the caller supplies what it knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantFacts {
    pub arch: Arch,
    pub graphics: GraphicsApi,
    pub dependencies: Vec<DependencySpec>,
    pub windows_version: WindowsVersion,
}

/// Build the ordered variant chain from explicit facts.
pub fn build_variants_from(facts: VariantFacts, config: &Config) -> Vec<RuntimeEnv> {
    let VariantFacts {
        arch,
        graphics,
        dependencies,
        windows_version,
    } = facts;

    let preferred = config.wine_variant.label().to_string();
    let mut variants: Vec<RuntimeEnv> = Vec::new();

    let mut add = |v: RuntimeEnv| {
        if !variants.iter().any(|e| e.signature() == v.signature()) {
            variants.push(v);
        }
    };

    // 1. The user's preferred Wine build with the full translation stack.
    add(RuntimeEnv {
        wine_build: preferred.clone(),
        arch,
        windows_version,
        // DXVK only makes sense when the application actually imports Direct3D;
        // installing it into an unrelated prefix just adds failure surface.
        dxvk: config.dxvk && graphics.uses_dxvk(),
        vkd3d_proton: config.vkd3d_proton && graphics.uses_vkd3d(),
        dll_overrides: graphics_overrides(graphics, config),
        env: Vec::new(),
        dependencies: dependencies.clone(),
        rationale: format!(
            "preferred build ({preferred}), {}",
            if graphics == GraphicsApi::None {
                "no 3D translation".to_string()
            } else {
                format!("{} via Vulkan", graphics.label())
            }
        ),
    });

    // 2. Wine staging, which carries patches for newer games.
    add(RuntimeEnv {
        wine_build: "staging".to_string(),
        arch,
        windows_version,
        dxvk: config.dxvk && graphics.uses_dxvk(),
        vkd3d_proton: config.vkd3d_proton && graphics.uses_vkd3d(),
        dll_overrides: graphics_overrides(graphics, config),
        env: Vec::new(),
        dependencies: dependencies.clone(),
        rationale: "Wine staging, for newer patches".to_string(),
    });

    // 3. An older Windows version, for software from the XP era.
    add(RuntimeEnv {
        wine_build: "stable".to_string(),
        arch,
        windows_version: WindowsVersion::WinXp,
        dxvk: config.dxvk && graphics.uses_dxvk(),
        vkd3d_proton: false,
        dll_overrides: graphics_overrides(graphics, config),
        env: Vec::new(),
        dependencies: dependencies.clone(),
        rationale: "Windows XP mode, for older software".to_string(),
    });

    // 4. Last resort: no translation layers at all. If DXVK itself is the
    //    problem, Wine's own Direct3D implementation may still work.
    add(RuntimeEnv {
        wine_build: "system".to_string(),
        arch,
        windows_version: WindowsVersion::Win7,
        dxvk: false,
        vkd3d_proton: false,
        dll_overrides: Vec::new(),
        env: Vec::new(),
        dependencies: dependencies.clone(),
        rationale: "system Wine without graphics translation (last resort)".to_string(),
    });

    variants
}

/// `WINEDLLOVERRIDES` entries that make the translation layers take precedence
/// over Wine's built-in implementations.
fn graphics_overrides(graphics: GraphicsApi, config: &Config) -> Vec<String> {
    let mut overrides = Vec::new();
    if config.dxvk {
        for dll in ["d3d9", "d3d10core", "d3d11", "dxgi"] {
            overrides.push(format!("{dll}=n,b"));
        }
    }
    if config.vkd3d_proton && graphics.uses_vkd3d() {
        overrides.push("d3d12=n,b".to_string());
    }
    overrides
}

/// Build a profile purely from what the executable says about itself.
pub fn generic_profile(inspection: &PeInspection, config: &Config) -> AppProfile {
    let requirements = infer_requirements(inspection);
    let variants = build_variants(inspection, &requirements, config);

    let base_name = inspection
        .path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Windows Application".to_string());

    let id = format!("{}-{}", slugify(&base_name), inspection.arch.bits());

    let mut notes = requirements.notes.clone();
    if !inspection.imports_readable {
        notes.push(
            "The file could not be fully parsed, so dependencies may be incomplete. If the \
             application fails to start, install its missing runtime from the application's \
             settings."
                .to_string(),
        );
    }

    AppProfile {
        id,
        name: clean_app_name(&base_name),
        version: String::new(),
        hashes: vec![inspection.sha256.clone()],
        arch: Some(inspection.arch),
        requirements,
        variants,
        source: ProfileSource::Generated,
        installer_args: Vec::new(),
        main_exe_hint: None,
        notes: notes.join(". "),
        updated_at: None,
    }
}

/// Build a profile for an input that cannot be inspected, such as an `.msi`
/// package or a `.bat` script.
///
/// The architecture defaults to whatever the caller passes; a modern Wine build
/// handles 32-bit code inside a 64-bit prefix through WoW64.
pub fn fallback_profile(
    name: &str,
    arch: Arch,
    sha256: &str,
    notes: Vec<String>,
    config: &Config,
) -> AppProfile {
    let facts = VariantFacts {
        arch,
        graphics: GraphicsApi::None,
        dependencies: vec![DependencySpec::new("corefonts", "standard Windows fonts").optional()],
        windows_version: WindowsVersion::Win10,
    };
    let variants = build_variants_from(facts, config);

    let mut notes = notes;
    notes.push(
        "WinDrop could not read a Windows executable header from this file, so the \
         compatibility settings are generic."
            .to_string(),
    );

    AppProfile {
        id: format!("{}-{}", slugify(name), arch.bits()),
        name: clean_app_name(name),
        version: String::new(),
        hashes: vec![sha256.to_string()],
        arch: Some(arch),
        requirements: Requirements::default(),
        variants,
        source: ProfileSource::Generated,
        installer_args: Vec::new(),
        main_exe_hint: None,
        notes: notes.join(". "),
        updated_at: None,
    }
}

#[cfg(test)]
mod tests {
    // Setting one field on a default is the clearest way to say "defaults,
    // except this"; the lint is aimed at production code, where it usually
    // means a missing derive.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::fixtures::{self, Arch, PeSpec};
    use std::path::Path;

    fn inspect(spec: &PeSpec) -> PeInspection {
        let bytes = fixtures::synthetic_pe(spec);
        crate::compat::pe::inspect_bytes(&bytes, Path::new("app.exe")).unwrap()
    }

    #[test]
    fn slugify_is_filesystem_safe_and_stable() {
        assert_eq!(slugify("Notepad++"), "notepad");
        assert_eq!(slugify("7-Zip 23.01"), "7-zip-23-01");
        assert_eq!(slugify("  Trim  me  "), "trim-me");
        assert_eq!(slugify("!!!"), "app");
        assert_eq!(slugify(""), "app");
        // Non-ASCII input must still produce a usable name, never panic.
        assert!(slugify("日本語アプリ")
            .chars()
            .all(|c| c.is_ascii_alphanumeric()));
        assert!(slugify(&"a".repeat(200)).len() <= 64);
    }

    #[test]
    fn graphics_api_is_inferred_from_imports() {
        assert_eq!(
            infer_requirements(&inspect(&PeSpec::example_d3d12_game())).graphics,
            Some(GraphicsApi::D3D12)
        );
        let d3d9 = PeSpec::example_console_tool().with_imports("d3d9", &["Direct3DCreate9"]);
        assert_eq!(
            infer_requirements(&inspect(&d3d9)).graphics,
            Some(GraphicsApi::D3D9)
        );
        let plain = PeSpec::example_console_tool();
        assert_eq!(infer_requirements(&inspect(&plain)).graphics, None);
    }

    #[test]
    fn d3d12_wins_over_a_statically_linked_d3d9() {
        let spec = PeSpec::example_d3d12_game().with_imports("d3d9", &["Direct3DCreate9"]);
        assert_eq!(
            infer_requirements(&inspect(&spec)).graphics,
            Some(GraphicsApi::D3D12)
        );
    }

    #[test]
    fn vc_runtime_imports_map_to_the_right_winetricks_verb() {
        let deps = infer_dependencies(&inspect(&PeSpec::example_installer()));
        let verbs: Vec<&str> = deps.iter().map(|d| d.verb.as_str()).collect();
        assert!(verbs.contains(&"vcrun2022"), "{verbs:?}");
    }

    #[test]
    fn only_the_newest_matching_vc_runtime_is_installed() {
        let spec = PeSpec::example_console_tool()
            .with_imports("msvcr120", &["_beginthreadex"])
            .with_imports("msvcr100", &["_beginthreadex"]);
        let deps = infer_dependencies(&inspect(&spec));
        let verbs: Vec<&str> = deps.iter().map(|d| d.verb.as_str()).collect();
        assert!(verbs.contains(&"vcrun2013"));
        assert!(verbs.contains(&"vcrun2010"));
        // Both are independent generations, so both appear exactly once.
        assert_eq!(verbs.iter().filter(|v| **v == "vcrun2013").count(), 1);
    }

    #[test]
    fn managed_binaries_get_dotnet() {
        let deps = infer_dependencies(&inspect(&PeSpec::example_dotnet_app()));
        assert!(deps.iter().any(|d| d.verb == "dotnet48"));
    }

    #[test]
    fn fonts_are_optional_and_not_added_for_console_tools() {
        let gui_deps = infer_dependencies(&inspect(&PeSpec::example_installer()));
        let fonts = gui_deps.iter().find(|d| d.verb == "corefonts");
        assert!(fonts.map(|f| f.optional).unwrap_or(false));

        let console_deps = infer_dependencies(&inspect(&PeSpec::example_console_tool()));
        assert!(!console_deps.iter().any(|d| d.verb == "corefonts"));
    }

    #[test]
    fn dll_dependencies_are_deduplicated() {
        let spec = PeSpec::example_console_tool()
            .with_imports("vcruntime140", &["memcpy"])
            .with_imports("msvcp140", &["std::x"])
            .with_imports("vcruntime140_1", &["__CxxFrameHandler4"]);
        let deps = infer_dependencies(&inspect(&spec));
        assert_eq!(deps.iter().filter(|d| d.verb == "vcrun2022").count(), 1);
    }

    #[test]
    fn xp_era_timestamps_select_windows_xp() {
        let mut spec = PeSpec::example_installer();
        spec.timestamp = 1_000_000_000; // 2001
        assert_eq!(
            guess_windows_version(&inspect(&spec)),
            WindowsVersion::WinXp
        );

        let mut spec = PeSpec::example_installer();
        spec.timestamp = 1_700_000_000; // 2023
        assert_eq!(
            guess_windows_version(&inspect(&spec)),
            WindowsVersion::Win10
        );

        // .NET refuses to install on an XP-claiming prefix.
        let mut spec = PeSpec::example_dotnet_app();
        spec.timestamp = 1_000_000_000;
        assert_eq!(guess_windows_version(&inspect(&spec)), WindowsVersion::Win7);
    }

    #[test]
    fn chain_starts_with_the_preferred_build_and_degrades() {
        let config = Config::default();
        let inspection = inspect(&PeSpec::example_d3d12_game());
        let req = infer_requirements(&inspection);
        let chain = build_variants(&inspection, &req, &config);

        assert_eq!(chain[0].wine_build, "stable");
        assert!(chain[0].vkd3d_proton, "D3D12 needs VKD3D-Proton");
        assert!(!chain[0].dxvk, "DXVK is for D3D9/10/11 only");
        assert!(chain[0].dll_overrides.iter().any(|o| o == "d3d12=n,b"));
        assert_eq!(chain.len(), 4);
        // The last resort must not use any translation layer.
        let last = chain.last().unwrap();
        assert!(!last.dxvk && !last.vkd3d_proton);
        assert!(last.dll_overrides.is_empty());
    }

    #[test]
    fn chain_respects_the_configured_wine_variant() {
        let mut config = Config::default();
        config.wine_variant = WineVariant::Staging;
        let inspection = inspect(&PeSpec::example_installer());
        let req = infer_requirements(&inspection);
        let chain = build_variants(&inspection, &req, &config);
        assert_eq!(chain[0].wine_build, "staging");
        // The explicit staging step de-duplicates against the preferred one.
        assert_eq!(
            chain.iter().filter(|v| v.wine_build == "staging").count(),
            1
        );
    }

    #[test]
    fn disabling_dxvk_removes_it_from_every_variant() {
        let mut config = Config::default();
        config.dxvk = false;
        let inspection = inspect(&PeSpec::example_d3d12_game());
        let req = infer_requirements(&inspection);
        for v in build_variants(&inspection, &req, &config) {
            assert!(!v.dxvk);
        }
    }

    #[test]
    fn the_configured_graphics_stack_is_permitted_but_absent_when_not_needed() {
        let config = Config::default();
        // A plain console tool does not need the D3D11 layer.
        let inspection = inspect(&PeSpec::example_console_tool());
        let req = infer_requirements(&inspection);
        let chain = build_variants(&inspection, &req, &config);
        assert!(!chain[0].vkd3d_proton);
    }

    #[test]
    fn every_variant_carries_the_dependency_list() {
        let config = Config::default();
        let inspection = inspect(&PeSpec::example_installer());
        let req = infer_requirements(&inspection);
        let chain = build_variants(&inspection, &req, &config);
        assert!(chain.iter().all(|v| !v.dependencies.is_empty()));
        assert!(chain
            .iter()
            .all(|v| v.dependencies.iter().any(|d| d.verb == "vcrun2022")));
    }

    #[test]
    fn generated_profile_records_the_hash_and_arch() {
        let config = Config::default();
        let inspection = inspect(&PeSpec::example_d3d12_game());
        let profile = generic_profile(&inspection, &config);
        assert!(profile.matches_hash(&inspection.sha256));
        assert_eq!(profile.arch, Some(Arch::X86_64));
        assert_eq!(profile.source, ProfileSource::Generated);
        assert_eq!(profile.id, "app-64");
        profile.validate().unwrap();
    }

    #[test]
    fn a_profile_with_no_variants_is_rejected() {
        let mut profile = AppProfile {
            id: "broken".into(),
            name: "Broken".into(),
            version: String::new(),
            hashes: vec![],
            arch: None,
            requirements: Requirements::default(),
            variants: vec![],
            source: ProfileSource::Local,
            installer_args: vec![],
            main_exe_hint: None,
            notes: String::new(),
            updated_at: None,
        };
        assert!(matches!(profile.validate(), Err(Error::Config { .. })));

        profile.variants.push(RuntimeEnv {
            wine_build: "stable".into(),
            arch: Arch::Arm64,
            windows_version: WindowsVersion::Win10,
            dxvk: false,
            vkd3d_proton: false,
            dll_overrides: vec![],
            env: vec![],
            dependencies: vec![],
            rationale: String::new(),
        });
        assert!(profile.validate().is_err(), "ARM64 is not supported yet");
    }

    #[test]
    fn profiles_round_trip_through_json() {
        let config = Config::default();
        let profile = generic_profile(&inspect(&PeSpec::example_installer()), &config);
        let json = profile.to_json_pretty().unwrap();
        assert_eq!(AppProfile::from_json(&json).unwrap(), profile);
    }

    #[test]
    fn hash_matching_is_case_insensitive() {
        let mut profile =
            generic_profile(&inspect(&PeSpec::example_installer()), &Config::default());
        profile.hashes = vec!["ABCDEF".into()];
        assert!(profile.matches_hash("abcdef"));
        assert!(!profile.matches_hash("fedcba"));
    }

    #[test]
    fn signatures_distinguish_variants_that_behave_differently() {
        let base = generic_profile(&inspect(&PeSpec::example_installer()), &Config::default())
            .variants[0]
            .clone();

        // Every execution-affecting field must change the signature, otherwise
        // a remembered preference could match the wrong variant.
        let mutate: Vec<(&str, RuntimeEnv)> = vec![
            (
                "wine build",
                RuntimeEnv {
                    wine_build: "staging".into(),
                    ..base.clone()
                },
            ),
            (
                "architecture",
                RuntimeEnv {
                    arch: Arch::X86_64,
                    ..base.clone()
                },
            ),
            (
                "windows version",
                RuntimeEnv {
                    windows_version: WindowsVersion::WinXp,
                    ..base.clone()
                },
            ),
            (
                "dxvk",
                RuntimeEnv {
                    dxvk: !base.dxvk,
                    ..base.clone()
                },
            ),
            (
                "vkd3d",
                RuntimeEnv {
                    vkd3d_proton: !base.vkd3d_proton,
                    ..base.clone()
                },
            ),
            (
                "dll overrides",
                RuntimeEnv {
                    dll_overrides: vec!["d3d11=n,b".into()],
                    ..base.clone()
                },
            ),
            (
                "environment",
                RuntimeEnv {
                    env: vec![("VKD3D_CONFIG".into(), "dxr".into())],
                    ..base.clone()
                },
            ),
        ];
        for (what, variant) in mutate {
            assert_ne!(
                variant.signature(),
                base.signature(),
                "a different {what} must produce a different signature"
            );
        }
    }

    #[test]
    fn signatures_ignore_wording_and_ordering() {
        let base = generic_profile(&inspect(&PeSpec::example_installer()), &Config::default())
            .variants[0]
            .clone();

        // Re-wording a rationale is not a behavioural change.
        let reworded = RuntimeEnv {
            rationale: "completely different wording".into(),
            ..base.clone()
        };
        assert_eq!(reworded.signature(), base.signature());

        // Neither is reordering the environment or the override list.
        let mut shuffled = base.clone();
        shuffled.env = vec![("B".into(), "2".into()), ("A".into(), "1".into())];
        let mut same = base.clone();
        same.env = vec![("A".into(), "1".into()), ("B".into(), "2".into())];
        assert_eq!(shuffled.signature(), same.signature());
    }

    #[test]
    fn generated_chains_never_repeat_an_environment() {
        for spec in [
            PeSpec::example_installer(),
            PeSpec::example_d3d12_game(),
            PeSpec::example_console_tool(),
        ] {
            let inspection = inspect(&spec);
            let req = infer_requirements(&inspection);
            let chain = build_variants(&inspection, &req, &Config::default());
            let mut signatures: Vec<String> = chain.iter().map(|v| v.signature()).collect();
            let total = signatures.len();
            signatures.sort();
            signatures.dedup();
            assert_eq!(
                signatures.len(),
                total,
                "the chain for {spec:?} contains a duplicated environment"
            );
        }
    }

    #[test]
    fn dll_override_value_is_none_when_empty() {
        let inspection = inspect(&PeSpec::example_d3d12_game());
        let req = infer_requirements(&inspection);
        let chain = build_variants(&inspection, &req, &Config::default());
        assert!(chain.last().unwrap().dll_overrides_value().is_none());
        assert!(chain[0]
            .dll_overrides_value()
            .unwrap()
            .contains("d3d12=n,b"));
    }
}

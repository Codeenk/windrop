//! Graphics translation components: DXVK and VKD3D-Proton.
//!
//! Both ship as archives laid out per bitness:
//!
//! ```text
//! dxvk-2.4/
//! ├── x64/  d3d9.dll d3d10core.dll d3d11.dll dxgi.dll
//! └── x32/  d3d9.dll d3d10core.dll d3d11.dll dxgi.dll
//!
//! vkd3d-proton-2.13/
//! ├── x64/  d3d12.dll d3d12core.dll
//! └── x86/  d3d12.dll d3d12core.dll
//! ```
//!
//! Installing a component means copying the right bitness into the prefix's
//! `system32` (64-bit) or `syswow64` (32-bit) directory, which is exactly what
//! Wine's native-DLL override then picks up.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::compat::pe::Arch;
use crate::runtime::prefix::PrefixPaths;
use crate::{Error, Result};

/// Which translation layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentKind {
    /// Direct3D 9/10/11 to Vulkan.
    Dxvk,
    /// Direct3D 12 to Vulkan.
    Vkd3dProton,
}

impl ComponentKind {
    /// Directory name under `<data>/runtime/`.
    pub fn dir_name(self) -> &'static str {
        match self {
            ComponentKind::Dxvk => "dxvk",
            ComponentKind::Vkd3dProton => "vkd3d-proton",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ComponentKind::Dxvk => "DXVK",
            ComponentKind::Vkd3dProton => "VKD3D-Proton",
        }
    }

    /// The version a fresh install uses when none is pinned.
    pub fn default_version(self) -> &'static str {
        match self {
            ComponentKind::Dxvk => "2.4",
            ComponentKind::Vkd3dProton => "2.13",
        }
    }

    /// DLLs the component must provide for an installation to count as valid.
    pub fn required_dlls(self) -> &'static [&'static str] {
        match self {
            ComponentKind::Dxvk => &["d3d9.dll", "d3d11.dll", "dxgi.dll"],
            ComponentKind::Vkd3dProton => &["d3d12.dll"],
        }
    }

    /// Environment variable DXVK reads for its configuration file.
    pub fn config_env_var(self) -> Option<&'static str> {
        match self {
            ComponentKind::Dxvk => Some("DXVK_CONFIG_FILE"),
            ComponentKind::Vkd3dProton => None,
        }
    }
}

/// A component version unpacked on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    pub kind: ComponentKind,
    pub version: String,
    /// Directory holding the bitness subdirectories.
    pub root: PathBuf,
}

impl Component {
    /// Find an unpacked component under a runtime directory.
    ///
    /// `runtime_dir` is `<data>/runtime`, so the search starts at
    /// `<data>/runtime/<kind>/<version>`. Release archives wrap their payload in
    /// an extra top-level directory (`dxvk-2.4/x64/...`), so the search descends
    /// through single-child directories until it finds one that actually holds
    /// bitness subdirectories.
    pub fn discover(runtime_dir: &Path, kind: ComponentKind, version: &str) -> Option<Self> {
        let base = runtime_dir.join(kind.dir_name());
        let preferred = base.join(version);

        let start = if preferred.is_dir() {
            preferred
        } else {
            // A single installed version is unambiguous, so accept it even when
            // the directory name does not match the requested version.
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            match dirs.len() {
                1 => dirs.remove(0),
                _ => return None,
            }
        };

        let root = find_component_root(&start, 0)?;
        Some(Component {
            kind,
            version: version.to_string(),
            root,
        })
    }

    /// The subdirectory holding DLLs of a given bitness.
    ///
    /// Both projects always ship a bitness split, so a root without one is
    /// treated as unusable rather than guessed at.
    pub fn bitness_dir(&self, arch: Arch) -> Option<PathBuf> {
        let names: &[&str] = if arch == Arch::X86 {
            &["x32", "x86", "i386", "32"]
        } else {
            &["x64", "x86_64", "amd64", "64"]
        };
        names.iter().map(|n| self.root.join(n)).find(|p| p.is_dir())
    }

    /// True when the component has something to install for this architecture.
    pub fn supports(&self, arch: Arch) -> bool {
        self.bitness_dir(arch).is_some()
    }

    /// Copy the component into a prefix.
    ///
    /// Returns the files written. A component that provides nothing for the
    /// requested architecture is not an error: DXVK is simply not applied.
    pub fn install_into(&self, prefix: &PrefixPaths, arch: Arch) -> Result<Vec<PathBuf>> {
        let Some(source) = self.bitness_dir(arch) else {
            tracing::debug!(
                component = self.kind.label(),
                arch = %arch,
                "no matching bitness directory; skipping"
            );
            return Ok(Vec::new());
        };
        let target = if arch == Arch::X86 {
            prefix.syswow64()
        } else {
            prefix.system32()
        };
        std::fs::create_dir_all(&target)?;

        let mut written = Vec::new();
        for entry in std::fs::read_dir(&source)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Only DLLs belong in the system directory; config files and
            // documentation stay out of the way.
            let is_dll = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("dll"))
                .unwrap_or(false);
            if !is_dll {
                continue;
            }
            let dest = target.join(entry.file_name());
            std::fs::copy(&path, &dest).map_err(Error::Io)?;
            written.push(dest);
        }
        written.sort();
        tracing::info!(
            component = self.kind.label(),
            version = %self.version,
            arch = %arch,
            files = written.len(),
            "installed translation layer"
        );
        Ok(written)
    }

    /// Verify the component supplies what the kind promises.
    pub fn validate(&self, arch: Arch) -> Result<()> {
        let Some(dir) = self.bitness_dir(arch) else {
            return Err(Error::InstallIncomplete {
                rationale: format!(
                    "{} {} has no {arch} binaries",
                    self.kind.label(),
                    self.version
                ),
            });
        };
        for dll in self.kind.required_dlls() {
            if !dir.join(dll).is_file() {
                return Err(Error::InstallIncomplete {
                    rationale: format!(
                        "{} {} is missing {dll} for {arch}",
                        self.kind.label(),
                        self.version
                    ),
                });
            }
        }
        Ok(())
    }

    pub fn display(&self) -> String {
        format!("{} {}", self.kind.label(), self.version)
    }
}

/// Directory names that indicate a bitness split.
const BITNESS_DIRS: &[&str] = &["x64", "x86_64", "amd64", "64", "x32", "x86", "i386", "32"];

/// Walk down through single-child directories until a bitness split is found.
fn find_component_root(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 3 {
        return None;
    }
    if BITNESS_DIRS.iter().any(|name| dir.join(name).is_dir()) {
        return Some(dir.to_path_buf());
    }
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    match subdirs.len() {
        1 => find_component_root(&subdirs[0], depth + 1),
        // Ambiguous: several subdirectories and none of them is a bitness split.
        _ => None,
    }
}

/// Which components a set of runtime flags requires.
pub fn required_components(dxvk: bool, vkd3d_proton: bool) -> Vec<ComponentKind> {
    let mut kinds = Vec::new();
    if dxvk {
        kinds.push(ComponentKind::Dxvk);
    }
    if vkd3d_proton {
        kinds.push(ComponentKind::Vkd3dProton);
    }
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count entries in a directory that may not exist at all.
    fn file_count(dir: &Path) -> usize {
        std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
    }

    fn fake_dxvk(dir: &Path, version: &str) -> PathBuf {
        let root = dir.join("dxvk").join(version);
        std::fs::create_dir_all(root.join("x64")).unwrap();
        std::fs::create_dir_all(root.join("x32")).unwrap();
        for dll in ["d3d9.dll", "d3d10core.dll", "d3d11.dll", "dxgi.dll"] {
            std::fs::write(root.join("x64").join(dll), b"fake 64").unwrap();
            std::fs::write(root.join("x32").join(dll), b"fake 32").unwrap();
        }
        std::fs::write(root.join("LICENSE"), "licence text").unwrap();
        root
    }

    fn fake_vkd3d(dir: &Path, version: &str) -> PathBuf {
        let root = dir.join("vkd3d-proton").join(version);
        std::fs::create_dir_all(root.join("x64")).unwrap();
        std::fs::create_dir_all(root.join("x86")).unwrap();
        for arch_dir in ["x64", "x86"] {
            std::fs::write(root.join(arch_dir).join("d3d12.dll"), b"fake").unwrap();
            std::fs::write(root.join(arch_dir).join("d3d12core.dll"), b"fake").unwrap();
        }
        root
    }

    #[test]
    fn a_component_is_discovered_by_kind_and_version() {
        let tmp = tempfile::tempdir().unwrap();
        fake_dxvk(tmp.path(), "2.4");
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();
        assert_eq!(component.version, "2.4");
        assert_eq!(component.kind, ComponentKind::Dxvk);
        assert_eq!(component.display(), "DXVK 2.4");
    }

    #[test]
    fn an_extra_wrapping_directory_is_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let wrapped = tmp
            .path()
            .join("dxvk")
            .join("2.4")
            .join("dxvk-2.4")
            .join("x64");
        std::fs::create_dir_all(&wrapped).unwrap();
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();
        assert!(component.root.ends_with("dxvk-2.4"));
    }

    #[test]
    fn a_missing_component_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").is_none());
    }

    #[test]
    fn a_single_installed_version_is_used_when_the_name_does_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        fake_dxvk(tmp.path(), "2.3.1");
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();
        assert_eq!(
            component.version, "2.4",
            "the requested version is recorded"
        );
        assert!(component.root.ends_with("2.3.1"));
    }

    #[test]
    fn bitness_directories_are_recognised_for_both_namings() {
        let tmp = tempfile::tempdir().unwrap();
        fake_dxvk(tmp.path(), "2.4");
        let dxvk = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();
        assert!(dxvk.bitness_dir(Arch::X86_64).unwrap().ends_with("x64"));
        assert!(dxvk.bitness_dir(Arch::X86).unwrap().ends_with("x32"));

        fake_vkd3d(tmp.path(), "2.13");
        let vkd3d = Component::discover(tmp.path(), ComponentKind::Vkd3dProton, "2.13").unwrap();
        assert!(vkd3d.bitness_dir(Arch::X86_64).unwrap().ends_with("x64"));
        // VKD3D-Proton calls its 32-bit directory x86.
        assert!(vkd3d.bitness_dir(Arch::X86).unwrap().ends_with("x86"));
    }

    #[test]
    fn installation_copies_64_bit_dlls_into_system32() {
        let tmp = tempfile::tempdir().unwrap();
        fake_dxvk(tmp.path(), "2.4");
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();

        let prefix = PrefixPaths::from_root(tmp.path().join("prefix"));
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();

        let written = component.install_into(&prefix, Arch::X86_64).unwrap();
        assert_eq!(written.len(), 4);
        assert!(prefix.system32().join("d3d11.dll").is_file());
        assert!(prefix.system32().join("dxgi.dll").is_file());
        // Config files must not be copied into the system directory.
        assert!(!prefix.system32().join("LICENSE").exists());
        assert_eq!(
            file_count(&prefix.syswow64()),
            0,
            "32-bit dir must stay empty"
        );
    }

    #[test]
    fn installation_copies_32_bit_dlls_into_syswow64() {
        let tmp = tempfile::tempdir().unwrap();
        fake_dxvk(tmp.path(), "2.4");
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();

        let prefix = PrefixPaths::from_root(tmp.path().join("prefix"));
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();

        let written = component.install_into(&prefix, Arch::X86).unwrap();
        assert_eq!(written.len(), 4);
        assert!(prefix.syswow64().join("d3d11.dll").is_file());
        assert_eq!(
            file_count(&prefix.system32()),
            0,
            "64-bit dir must stay empty"
        );
    }

    #[test]
    fn installing_a_component_without_matching_bitness_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        // 64-bit DLLs only.
        let root = tmp.path().join("dxvk").join("2.4").join("x64");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("d3d11.dll"), b"x").unwrap();
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();

        let prefix = PrefixPaths::from_root(tmp.path().join("prefix"));
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();

        assert!(component
            .install_into(&prefix, Arch::X86)
            .unwrap()
            .is_empty());
        assert_eq!(file_count(&prefix.syswow64()), 0);
    }

    #[test]
    fn validation_catches_an_incomplete_component() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("dxvk").join("2.4").join("x64");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("d3d9.dll"), b"x").unwrap(); // dxgi/d3d11 missing

        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();
        match component.validate(Arch::X86_64) {
            Err(Error::InstallIncomplete { rationale }) => assert!(rationale.contains("d3d11.dll")),
            other => panic!("expected InstallIncomplete, got {other:?}"),
        }
    }

    #[test]
    fn validation_passes_for_a_complete_component() {
        let tmp = tempfile::tempdir().unwrap();
        fake_dxvk(tmp.path(), "2.4");
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();
        component.validate(Arch::X86_64).unwrap();
        component.validate(Arch::X86).unwrap();

        fake_vkd3d(tmp.path(), "2.13");
        let vkd3d = Component::discover(tmp.path(), ComponentKind::Vkd3dProton, "2.13").unwrap();
        vkd3d.validate(Arch::X86_64).unwrap();
    }

    #[test]
    fn reinstallation_overwrites_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        fake_dxvk(tmp.path(), "2.4");
        let component = Component::discover(tmp.path(), ComponentKind::Dxvk, "2.4").unwrap();
        let prefix = PrefixPaths::from_root(tmp.path().join("prefix"));
        crate::runtime::prefix::prepare_directories(&prefix).unwrap();

        component.install_into(&prefix, Arch::X86_64).unwrap();
        // A stale Wine-provided file must be replaced, not duplicated.
        std::fs::write(prefix.system32().join("d3d11.dll"), b"wine builtin").unwrap();
        component.install_into(&prefix, Arch::X86_64).unwrap();
        assert_eq!(
            std::fs::read(prefix.system32().join("d3d11.dll")).unwrap(),
            b"fake 64"
        );
    }

    #[test]
    fn required_components_track_the_runtime_flags() {
        assert!(required_components(false, false).is_empty());
        assert_eq!(required_components(true, false), vec![ComponentKind::Dxvk]);
        assert_eq!(
            required_components(false, true),
            vec![ComponentKind::Vkd3dProton]
        );
        assert_eq!(
            required_components(true, true),
            vec![ComponentKind::Dxvk, ComponentKind::Vkd3dProton]
        );
    }

    #[test]
    fn config_env_var_is_specific_to_dxvk() {
        assert_eq!(
            ComponentKind::Dxvk.config_env_var(),
            Some("DXVK_CONFIG_FILE")
        );
        assert_eq!(ComponentKind::Vkd3dProton.config_env_var(), None);
    }
}

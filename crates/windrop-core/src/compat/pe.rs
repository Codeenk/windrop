//! Portable Executable inspection.
//!
//! Two sources of truth, deliberately layered:
//!
//! 1. **Header fields** (`machine`, subsystem, characteristics, data
//!    directories) are read by hand from fixed offsets. This never fails on an
//!    otherwise well-formed image, which matters because packed, obfuscated and
//!    hand-crafted installers routinely defeat strict full-image parsers.
//! 2. **The import table** comes from [`goblin`]. If goblin refuses the file we
//!    keep the header facts and record that imports were unavailable instead of
//!    rejecting the executable outright.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

// --------------------------------------------------------------- constants

pub const MACHINE_I386: u16 = 0x014C;
pub const MACHINE_ARM: u16 = 0x01C0;
pub const MACHINE_ARM64: u16 = 0xAA64;
pub const MACHINE_AMD64: u16 = 0x8664;

pub const IMAGE_FILE_RELOCS_STRIPPED: u16 = 0x0001;
pub const IMAGE_FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
pub const IMAGE_FILE_LARGE_ADDRESS_AWARE: u16 = 0x0020;
pub const IMAGE_FILE_32BIT_MACHINE: u16 = 0x0100;
pub const IMAGE_FILE_DLL: u16 = 0x2000;

pub const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;
pub const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;

pub const PE_MAGIC_32: u16 = 0x10B;
pub const PE_MAGIC_64: u16 = 0x20B;

/// Data-directory index of the import table.
pub const DIR_IMPORT: usize = 1;
/// Data-directory index of the certificate table.
pub const DIR_SECURITY: usize = 4;
/// Data-directory index of the CLR (.NET) runtime header.
pub const DIR_COM_DESCRIPTOR: usize = 14;

/// How much of a file is read for parsing. Import tables live near the start of
/// every real binary; hashing always covers the whole file.
const PARSE_WINDOW: u64 = 4 * 1024 * 1024;

// -------------------------------------------------------------------- Arch

/// Bitness of a Windows binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    /// 32-bit x86.
    X86,
    /// 64-bit x86.
    X86_64,
    /// 64-bit ARM. Detected so we can explain why it is unsupported.
    Arm64,
}

impl Arch {
    pub fn machine(self) -> u16 {
        match self {
            Arch::X86 => MACHINE_I386,
            Arch::X86_64 => MACHINE_AMD64,
            Arch::Arm64 => MACHINE_ARM64,
        }
    }

    pub fn is_64_bit(self) -> bool {
        matches!(self, Arch::X86_64 | Arch::Arm64)
    }

    /// Bit width, for display and for choosing a Wine binary.
    pub fn bits(self) -> u8 {
        if self.is_64_bit() {
            64
        } else {
            32
        }
    }

    /// True when WinDrop can run this natively with stock Wine builds.
    pub fn is_supported(self) -> bool {
        matches!(self, Arch::X86 | Arch::X86_64)
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Arch::X86 => "x86 (32-bit)",
            Arch::X86_64 => "x86_64 (64-bit)",
            Arch::Arm64 => "ARM64",
        })
    }
}

/// Human-readable name for a COFF machine constant.
pub fn machine_name(machine: u16) -> &'static str {
    match machine {
        MACHINE_I386 => "x86 (32-bit)",
        MACHINE_AMD64 => "x86_64 (64-bit)",
        MACHINE_ARM => "ARM (32-bit)",
        MACHINE_ARM64 => "ARM64",
        _ => "unknown",
    }
}

/// Map a COFF machine constant onto [`Arch`]. `None` for exotic targets.
pub fn arch_from_machine(machine: u16) -> Option<Arch> {
    match machine {
        MACHINE_I386 => Some(Arch::X86),
        MACHINE_AMD64 => Some(Arch::X86_64),
        MACHINE_ARM64 => Some(Arch::Arm64),
        _ => None,
    }
}

// ------------------------------------------------------------- inspection

/// One imported DLL and the functions pulled from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedLibrary {
    /// Normalised, lower-case, with the `.dll` suffix.
    pub dll: String,
    pub functions: Vec<String>,
}

impl ImportedLibrary {
    /// The name without its extension, upper-case: `KERNEL32`.
    pub fn base_name(&self) -> String {
        self.dll.trim_end_matches(".dll").to_ascii_uppercase()
    }
}

/// Everything WinDrop learns about a Windows binary before deciding how to run
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeInspection {
    /// Raw COFF machine constant.
    pub machine: u16,
    pub arch: Arch,
    /// `true` for a GUI-subsystem image.
    pub gui: bool,
    pub subsystem: u16,
    /// `IMAGE_FILE_DLL`: a library, not something the user can install.
    pub is_dll: bool,
    /// A managed (.NET) assembly, by CLR directory or `mscoree` import.
    pub dotnet: bool,
    /// The image carries an Authenticode certificate table.
    pub signed: bool,
    /// Imports, when the full-image parser accepted the file.
    pub imports: Vec<ImportedLibrary>,
    /// `false` when only header fields could be read. Not an error.
    pub imports_readable: bool,
    /// Linker timestamp, exposed as a build-date hint.
    pub timestamp: u32,
    pub size_bytes: u64,
    /// Lower-case hex SHA-256 of the whole file.
    pub sha256: String,
    /// The file this was read from, when there was one.
    pub path: Option<PathBuf>,
}

impl PeInspection {
    /// Total number of imported functions across all libraries.
    pub fn imported_function_count(&self) -> usize {
        self.imports.iter().map(|l| l.functions.len()).sum()
    }

    /// True when `dll` (with or without `.dll`) is imported.
    pub fn imports_dll(&self, dll: &str) -> bool {
        let want = dll.trim_end_matches(".dll").to_ascii_lowercase();
        self.imports
            .iter()
            .any(|l| l.base_name().to_ascii_lowercase() == want)
    }

    /// A one-line summary for CLI output.
    pub fn summary(&self) -> String {
        let kind = if self.is_dll {
            "DLL"
        } else if self.gui {
            "GUI application"
        } else {
            "console application"
        };
        let mut s = format!("{kind}, {arch}", arch = self.arch);
        if self.dotnet {
            s.push_str(", .NET");
        }
        if self.signed {
            s.push_str(", signed");
        }
        s.push_str(&format!(", {} import(s)", self.imported_function_count()));
        if !self.imports_readable {
            s.push_str(" (import table unreadable)");
        }
        s
    }
}

/// Inspect a Windows executable on disk.
pub fn inspect(path: &Path) -> Result<PeInspection> {
    let meta = std::fs::metadata(path).map_err(|_| Error::InputMissing {
        path: path.to_path_buf(),
    })?;
    if !meta.is_file() {
        return Err(Error::InputMissing {
            path: path.to_path_buf(),
        });
    }
    let size = meta.len();

    let bytes = read_window(path, PARSE_WINDOW)?;
    let mut info = inspect_bytes(&bytes, path)?;
    info.size_bytes = size;

    // Hash the whole file, streaming so a multi-gigabyte installer does not have
    // to fit in memory.
    info.sha256 = sha256_file(path)?;
    Ok(info)
}

/// Inspect an in-memory image. `path` is only used for error messages.
pub fn inspect_bytes(bytes: &[u8], path: &Path) -> Result<PeInspection> {
    let headers = parse_headers(bytes, path)?;

    let (imports, imports_readable) = match goblin::pe::PE::parse(bytes) {
        Ok(pe) => {
            let mut libs: Vec<ImportedLibrary> = Vec::new();
            for imp in pe.imports {
                let dll = imp.dll.to_ascii_lowercase();
                match libs.iter_mut().find(|l| l.dll == dll) {
                    Some(existing) => existing.functions.push(imp.name.to_string()),
                    None => libs.push(ImportedLibrary {
                        dll,
                        functions: vec![imp.name.to_string()],
                    }),
                }
            }
            // goblin falls back to the import descriptors only; also fold in
            // the lazily-loaded libraries it reports.
            for lib in &pe.libraries {
                let dll = lib.to_ascii_lowercase();
                if !libs.iter().any(|l| l.dll == dll) {
                    libs.push(ImportedLibrary {
                        dll,
                        functions: Vec::new(),
                    });
                }
            }
            libs.sort_by(|a, b| a.dll.cmp(&b.dll));
            for l in &mut libs {
                l.functions.sort();
                l.functions.dedup();
            }
            (libs, true)
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "full PE parse failed; continuing with header fields only"
            );
            (Vec::new(), false)
        }
    };

    let dotnet =
        headers.com_descriptor_size > 0 || imports.iter().any(|l| l.base_name() == "MSCOREE");

    Ok(PeInspection {
        machine: headers.machine,
        arch: headers.arch,
        gui: headers.subsystem == IMAGE_SUBSYSTEM_WINDOWS_GUI,
        subsystem: headers.subsystem,
        is_dll: headers.is_dll,
        dotnet,
        signed: headers.certificate_size > 0,
        imports,
        imports_readable,
        timestamp: headers.timestamp,
        size_bytes: bytes.len() as u64,
        sha256: sha256_bytes(bytes),
        path: Some(path.to_path_buf()),
    })
}

struct RawHeaders {
    machine: u16,
    arch: Arch,
    subsystem: u16,
    is_dll: bool,
    timestamp: u32,
    com_descriptor_size: u32,
    certificate_size: u32,
}

/// Read the fields that matter straight out of the image bytes.
///
/// Every access is bounds-checked, so a truncated or hostile file produces a
/// clean [`Error::NotAPeFile`] rather than a panic.
fn parse_headers(bytes: &[u8], path: &Path) -> Result<RawHeaders> {
    let bad = |reason: &str| Error::NotAPeFile {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    };

    // Check the signature before the length: "this is not a Windows program" is
    // a far more useful message than "this file is 39 bytes".
    if bytes.len() < 2 {
        return Err(bad("the file is empty"));
    }
    if &bytes[0..2] != b"MZ" {
        return Err(bad("missing 'MZ' signature (not a Windows executable)"));
    }
    if bytes.len() < 0x40 {
        return Err(bad("the file is too small to contain a DOS header"));
    }
    let pe_at = u32_at(bytes, 0x3C).ok_or_else(|| bad("truncated DOS header"))? as usize;
    if pe_at + 24 > bytes.len() {
        return Err(bad("PE header offset points past the end of the file"));
    }
    if &bytes[pe_at..pe_at + 4] != b"PE\0\0" {
        return Err(bad("missing 'PE\\0\\0' signature"));
    }

    let coff = pe_at + 4;
    let machine = u16_at(bytes, coff).ok_or_else(|| bad("truncated COFF header"))?;
    let opt_size = u16_at(bytes, coff + 16).ok_or_else(|| bad("truncated COFF header"))? as usize;
    let characteristics = u16_at(bytes, coff + 18).ok_or_else(|| bad("truncated COFF header"))?;
    let timestamp = u32_at(bytes, coff + 4).ok_or_else(|| bad("truncated COFF header"))?;

    let opt = coff + 20;
    if opt_size < 2 || opt + opt_size > bytes.len() {
        return Err(bad("optional header extends past the end of the file"));
    }
    let magic = u16_at(bytes, opt).ok_or_else(|| bad("truncated optional header"))?;
    let pe_plus = match magic {
        PE_MAGIC_64 => true,
        PE_MAGIC_32 => false,
        other => {
            return Err(bad(&format!(
                "unrecognised optional-header magic 0x{other:04X}"
            )))
        }
    };

    // The subsystem field is at the same offset in both optional-header
    // flavours; the layouts diverge only after it.
    let subsystem = u16_at(bytes, opt + 68).ok_or_else(|| bad("truncated optional header"))?;

    let (dirs_at, dir_count_at) = if pe_plus {
        (opt + 0x70, opt + 108)
    } else {
        (opt + 0x60, opt + 92)
    };
    let dir_count = u32_at(bytes, dir_count_at).unwrap_or(0) as usize;
    let dir = |index: usize| -> (u32, u32) {
        if index >= dir_count || index >= 16 {
            return (0, 0);
        }
        let at = dirs_at + index * 8;
        (
            u32_at(bytes, at).unwrap_or(0),
            u32_at(bytes, at + 4).unwrap_or(0),
        )
    };

    // A recognised-but-unsupported architecture (ARM64) is reported as a fact,
    // not an error; `Arch::is_supported` lets callers decide. An *unknown*
    // machine constant really is an error, since nothing can be inferred.
    let arch = arch_from_machine(machine).ok_or_else(|| {
        Error::UnsupportedArch(format!(
            "{} (machine 0x{machine:04X})",
            machine_name(machine)
        ))
    })?;

    Ok(RawHeaders {
        machine,
        arch,
        subsystem,
        is_dll: characteristics & IMAGE_FILE_DLL != 0,
        timestamp,
        com_descriptor_size: dir(DIR_COM_DESCRIPTOR).1,
        certificate_size: dir(DIR_SECURITY).1,
    })
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_window(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.by_ref().take(limit).read_to_end(&mut buf)?;
    Ok(buf)
}

/// SHA-256 of a whole file, streamed.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// SHA-256 of an in-memory buffer.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, PeSpec};

    fn tmp_exe(spec: &PeSpec) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.exe");
        fixtures::write_exe(&path, spec).unwrap();
        (dir, path)
    }

    #[test]
    fn detects_64_bit_gui_application() {
        let (_d, path) = tmp_exe(&PeSpec::example_d3d12_game());
        let info = inspect(&path).unwrap();
        assert_eq!(info.arch, Arch::X86_64);
        assert_eq!(info.arch.bits(), 64);
        assert!(info.gui);
        assert!(!info.is_dll);
        assert!(!info.dotnet);
        assert!(info.imports_readable);
        assert!(info.imports_dll("d3d12"));
        assert!(info.imports_dll("D3D12.dll"));
    }

    #[test]
    fn detects_32_bit_installer_and_its_vc_runtime_imports() {
        let (_d, path) = tmp_exe(&PeSpec::example_installer());
        let info = inspect(&path).unwrap();
        assert_eq!(info.arch, Arch::X86);
        assert_eq!(info.arch.bits(), 32);
        assert!(info.imports_dll("VCRUNTIME140"));
        assert!(info.imports_dll("msvcp140.dll"));
    }

    #[test]
    fn console_tools_are_not_gui() {
        let (_d, path) = tmp_exe(&PeSpec::example_console_tool());
        let info = inspect(&path).unwrap();
        assert!(!info.gui);
        assert_eq!(info.subsystem, IMAGE_SUBSYSTEM_WINDOWS_CUI);
    }

    #[test]
    fn libraries_are_flagged_so_they_can_be_refused() {
        let (_d, path) = tmp_exe(&PeSpec::example_dll());
        let info = inspect(&path).unwrap();
        assert!(info.is_dll);
    }

    #[test]
    fn managed_assemblies_are_detected_by_the_clr_directory() {
        let (_d, path) = tmp_exe(&PeSpec::example_dotnet_app());
        let info = inspect(&path).unwrap();
        assert!(info.dotnet);
    }

    #[test]
    fn managed_assemblies_are_also_detected_by_mscoree_imports() {
        // Some obfuscators strip the CLR directory; the import survives.
        let spec = PeSpec::default().with_imports("mscoree", &["_CorExeMain"]);
        let (_d, path) = tmp_exe(&spec);
        let info = inspect(&path).unwrap();
        assert!(info.imports_readable);
        assert!(info.dotnet);
    }

    #[test]
    fn header_facts_survive_a_file_the_full_parser_rejects() {
        // This is the packing/obfuscation case: the import table cannot be read,
        // but WinDrop still knows the architecture and that it is managed.
        let (_d, path) = tmp_exe(&PeSpec::example_dotnet_app());
        let info = inspect(&path).unwrap();
        assert!(
            !info.imports_readable,
            "fixture is intentionally unparseable"
        );
        assert_eq!(info.arch, Arch::X86_64);
        assert!(info.dotnet);
        assert!(info.summary().contains("import table unreadable"));
    }

    #[test]
    fn non_pe_input_is_rejected_with_a_specific_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        fixtures::corrupt::write(&path, &fixtures::corrupt::not_a_pe()).unwrap();
        match inspect(&path) {
            Err(Error::NotAPeFile { reason, .. }) => assert!(reason.contains("MZ")),
            other => panic!("expected NotAPeFile, got {other:?}"),
        }
    }

    #[test]
    fn truncated_and_empty_inputs_are_rejected_without_panicking() {
        let dir = tempfile::tempdir().unwrap();

        let p = dir.path().join("empty.exe");
        fixtures::corrupt::write(&p, &fixtures::corrupt::empty()).unwrap();
        assert!(matches!(inspect(&p), Err(Error::NotAPeFile { .. })));

        let p = dir.path().join("truncated.exe");
        fixtures::corrupt::write(&p, &fixtures::corrupt::truncated_mz()).unwrap();
        assert!(matches!(inspect(&p), Err(Error::NotAPeFile { .. })));
    }

    #[test]
    fn bogus_pe_offset_is_rejected() {
        let mut bytes = fixtures::synthetic_pe(&PeSpec::example_installer());
        // Point e_lfanew far past the end of the buffer.
        bytes[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        match inspect_bytes(&bytes, Path::new("evil.exe")) {
            Err(Error::NotAPeFile { reason, .. }) => assert!(reason.contains("past the end")),
            other => panic!("expected NotAPeFile, got {other:?}"),
        }
    }

    #[test]
    fn arm_binaries_are_reported_but_marked_unsupported() {
        // Inspection reports facts; policy ("you cannot run this") belongs to
        // the engine, so `inspect` succeeds and flags the architecture.
        let mut bytes = fixtures::synthetic_pe(&PeSpec::example_installer());
        bytes[0x44..0x46].copy_from_slice(&MACHINE_ARM64.to_le_bytes());
        let info = inspect_bytes(&bytes, Path::new("arm.exe")).unwrap();
        assert_eq!(info.arch, Arch::Arm64);
        assert!(!info.arch.is_supported());
        assert_eq!(info.arch.bits(), 64);
    }

    #[test]
    fn unknown_machine_is_reported_not_guessed() {
        let mut bytes = fixtures::synthetic_pe(&PeSpec::example_installer());
        bytes[0x44..0x46].copy_from_slice(&0x1234u16.to_le_bytes());
        assert!(matches!(
            inspect_bytes(&bytes, Path::new("weird.exe")),
            Err(Error::UnsupportedArch(_))
        ));
    }

    #[test]
    fn hashing_is_over_the_whole_file_not_the_parse_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.exe");
        // Two files sharing a 4 MiB prefix but differing afterwards must hash
        // differently.
        let mut a = fixtures::synthetic_pe(&PeSpec::example_installer());
        a.resize(PARSE_WINDOW as usize + 10, 0);
        let mut b = a.clone();
        let last = b.len() - 1;
        b[last] = 0xAB;
        std::fs::write(&path, &a).unwrap();
        let ha = inspect(&path).unwrap().sha256;
        std::fs::write(&path, &b).unwrap();
        let hb = inspect(&path).unwrap().sha256;
        assert_ne!(ha, hb);
        assert_eq!(ha, sha256_bytes(&a));
    }

    #[test]
    fn size_and_path_are_recorded() {
        let (_d, path) = tmp_exe(&PeSpec::example_installer());
        let info = inspect(&path).unwrap();
        assert_eq!(info.size_bytes, std::fs::metadata(&path).unwrap().len());
        assert_eq!(info.path.as_deref(), Some(path.as_path()));
        assert_eq!(
            info.size_bytes,
            fixtures::synthetic_pe(&PeSpec::example_installer()).len() as u64
        );
    }

    #[test]
    fn missing_file_reports_input_missing() {
        assert!(matches!(
            inspect(Path::new("/definitely/not/here.exe")),
            Err(Error::InputMissing { .. })
        ));
    }

    #[test]
    fn directories_are_not_accepted_as_executables() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            inspect(dir.path()),
            Err(Error::InputMissing { .. })
        ));
    }

    #[test]
    fn summary_mentions_the_important_facts() {
        let (_d, path) = tmp_exe(&PeSpec::example_d3d12_game());
        let s = inspect(&path).unwrap().summary();
        assert!(s.contains("64-bit"), "{s}");
        assert!(s.contains("GUI"), "{s}");
    }

    #[test]
    fn signed_images_are_flagged() {
        let spec = PeSpec {
            signed: true,
            ..PeSpec::example_installer()
        };
        let bytes = fixtures::synthetic_pe(&spec);
        let info = inspect_bytes(&bytes, Path::new("signed.exe")).unwrap();
        assert!(info.signed);

        let bytes = fixtures::synthetic_pe(&PeSpec::example_installer());
        assert!(
            !inspect_bytes(&bytes, Path::new("plain.exe"))
                .unwrap()
                .signed
        );
    }
}

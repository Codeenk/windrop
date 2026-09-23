//! Deterministic Windows executable generator.
//!
//! WinDrop's most important logic — deciding bitness, guessing which runtimes an
//! application needs, and locating the real program after an install — all
//! operates on PE binaries. Testing that logic needs PE binaries, but a test
//! suite cannot depend on shipping signed Windows software, and CI cannot
//! depend on Wine.
//!
//! This module writes genuine, structurally valid PE images: DOS header, COFF
//! header, optional header with data directories, two sections, and a real
//! import table with an ILT/IAT pair for every imported function. The output
//! parses with the same [`goblin`] code path that handles real installers.
//!
//! It powers the crate's tests and the `windrop demo` command, which lets a
//! user exercise the whole pipeline before they own a Windows `.exe`.

use std::path::Path;

use crate::Result;

// The PE constants and `Arch` live in the production inspector so there is a
// single source of truth; they are re-exported here for fixture builders.
pub use crate::compat::pe::{
    Arch, DIR_COM_DESCRIPTOR, IMAGE_FILE_32BIT_MACHINE, IMAGE_FILE_DLL,
    IMAGE_FILE_EXECUTABLE_IMAGE, IMAGE_FILE_LARGE_ADDRESS_AWARE, IMAGE_FILE_RELOCS_STRIPPED,
    IMAGE_SUBSYSTEM_WINDOWS_CUI, IMAGE_SUBSYSTEM_WINDOWS_GUI, MACHINE_AMD64, MACHINE_ARM,
    MACHINE_ARM64, MACHINE_I386,
};

const FILE_ALIGN: usize = 0x200;
const SECTION_ALIGN: u32 = 0x1000;
const RDATA_RVA: u32 = 0x1000;
const TEXT_RVA: u32 = 0x2000;

/// What the image should look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeSpec {
    pub arch: Arch,
    /// `true` produces a GUI subsystem image, `false` a console one.
    pub gui: bool,
    pub dll: bool,
    /// `dll name` (without `.dll`) to the functions imported from it.
    pub imports: Vec<(String, Vec<String>)>,
    /// Mark the image as a managed (.NET) assembly.
    pub dotnet: bool,
    pub timestamp: u32,
    /// A valid Authenticode directory entry, to model a signed installer.
    pub signed: bool,
    /// Extra bytes appended to the code section, so a fixture can look like a
    /// real multi-megabyte program rather than a launcher stub.
    pub text_padding: usize,
}

impl Default for PeSpec {
    fn default() -> Self {
        PeSpec {
            arch: Arch::X86_64,
            gui: true,
            dll: false,
            imports: Vec::new(),
            dotnet: false,
            timestamp: 0x5F00_0000,
            signed: false,
            text_padding: 0,
        }
    }
}

impl PeSpec {
    /// A 32-bit GUI installer that pulls in the VC++ runtime — the shape of a
    /// typical "Setup.exe".
    pub fn example_installer() -> Self {
        PeSpec {
            arch: Arch::X86,
            gui: true,
            imports: vec![
                (
                    "KERNEL32".into(),
                    vec!["GetProcAddress".into(), "LoadLibraryA".into()],
                ),
                ("USER32".into(), vec!["MessageBoxA".into()]),
                ("VCRUNTIME140".into(), vec!["memcpy".into()]),
                ("MSVCP140".into(), vec!["_Thrd_hardware_concurrency".into()]),
            ],
            ..Default::default()
        }
    }

    /// A 64-bit Direct3D 12 application.
    pub fn example_d3d12_game() -> Self {
        PeSpec {
            arch: Arch::X86_64,
            gui: true,
            imports: vec![
                ("KERNEL32".into(), vec!["CreateFileW".into()]),
                ("D3D12".into(), vec!["D3D12CreateDevice".into()]),
                ("dxgi".into(), vec!["CreateDXGIFactory2".into()]),
                ("VCRUNTIME140".into(), vec!["memcpy".into()]),
            ],
            ..Default::default()
        }
    }

    /// A 64-bit managed application.
    pub fn example_dotnet_app() -> Self {
        PeSpec {
            arch: Arch::X86_64,
            gui: true,
            imports: vec![
                ("KERNEL32".into(), vec!["GetModuleHandleW".into()]),
                ("mscoree".into(), vec!["_CorExeMain".into()]),
            ],
            dotnet: true,
            ..Default::default()
        }
    }

    /// A GUI application large enough to look like the real thing.
    ///
    /// Heuristics that pick the main program after an install weight a
    /// substantial binary; this fixture makes that signal testable without
    /// shipping a multi-megabyte asset.
    pub fn example_gui_large() -> Self {
        PeSpec {
            arch: Arch::X86_64,
            gui: true,
            imports: vec![("KERNEL32".into(), vec!["CreateWindowExW".into()])],
            text_padding: 1024 * 1024,
            ..Default::default()
        }
    }

    /// A console tool with no unusual dependencies.
    pub fn example_console_tool() -> Self {
        PeSpec {
            arch: Arch::X86_64,
            gui: false,
            imports: vec![("KERNEL32".into(), vec!["WriteFile".into()])],
            ..Default::default()
        }
    }

    /// A shared library, which WinDrop refuses to install on its own.
    pub fn example_dll() -> Self {
        PeSpec {
            arch: Arch::X86_64,
            gui: false,
            dll: true,
            imports: vec![("KERNEL32".into(), vec!["DisableThreadLibraryCalls".into()])],
            ..Default::default()
        }
    }

    pub fn with_arch(mut self, arch: Arch) -> Self {
        self.arch = arch;
        self
    }

    pub fn with_imports(mut self, dll: &str, funcs: &[&str]) -> Self {
        self.imports.push((
            dll.to_string(),
            funcs.iter().map(|s| s.to_string()).collect(),
        ));
        self
    }
}

/// Render the image.
pub fn synthetic_pe(spec: &PeSpec) -> Vec<u8> {
    let is64 = spec.arch.is_64_bit();
    let ptr = if is64 { 8 } else { 4 };
    let opt_size = if is64 { 0xF0 } else { 0xE0 };

    // ---------------------------------------------------------------- .rdata
    let mut rdata = SectionWriter::new(RDATA_RVA);
    let desc_off = rdata.reserve((spec.imports.len() + 1) * 20);

    // Each entry is a `(ilt, iat, name)` triple of *section offsets*, converted
    // to RVAs when the descriptor table is written.
    let mut descriptors: Vec<(usize, usize, usize)> = Vec::new();
    for (dll, funcs) in &spec.imports {
        rdata.align(ptr);
        let ilt = rdata.reserve(funcs.len() * ptr + ptr);
        let iat = rdata.reserve(funcs.len() * ptr + ptr);

        let mut entry_offsets = Vec::new();
        for func in funcs {
            rdata.align(2);
            let at = rdata.reserve(2 + func.len() + 1);
            rdata.patch_u16(at, 0); // hint
            rdata.patch_bytes(at + 2, func.as_bytes());
            entry_offsets.push(at);
        }

        // Populate both thunk arrays; leaving FirstThunk equal to
        // OriginalFirstThunk is what a static import table looks like on disk.
        for (j, entry) in entry_offsets.iter().enumerate() {
            let word = rdata.rva_of(*entry) as u64;
            rdata.patch_ptr(ilt + j * ptr, word, ptr);
            rdata.patch_ptr(iat + j * ptr, word, ptr);
        }

        let name = rdata.reserve(dll.len() + 5 + 1);
        rdata.patch_bytes(name, dll.as_bytes());
        rdata.patch_bytes(name + dll.len(), b".dll");

        descriptors.push((ilt, iat, name));
    }

    for (i, (ilt, iat, name)) in descriptors.iter().enumerate() {
        let at = desc_off + i * 20;
        rdata.patch_u32(at, rdata.rva_of(*ilt)); // OriginalFirstThunk
        rdata.patch_u32(at + 4, spec.timestamp);
        rdata.patch_u32(at + 8, 0); // ForwarderChain
        rdata.patch_u32(at + 12, rdata.rva_of(*name));
        rdata.patch_u32(at + 16, rdata.rva_of(*iat)); // FirstThunk
    }
    // The terminator descriptor is all zeroes and already reserved.

    let import_dir = (
        rdata.rva_of(desc_off),
        ((spec.imports.len() + 1) * 20) as u32,
    );
    let com_dir = if spec.dotnet {
        // A managed assembly is recognised by a non-empty CLR header directory.
        let at = rdata.reserve(72);
        rdata.patch_u32(at, 72); // cb
        (rdata.rva_of(at), 72u32)
    } else {
        (0, 0)
    };
    let security_dir = if spec.signed {
        // Certificate table entries are file offsets, not RVAs.
        (0u32, 0x100u32)
    } else {
        (0, 0)
    };

    let rdata_bytes = rdata.finish();
    let rdata_raw = align_up(rdata_bytes.len(), FILE_ALIGN);

    let text_bytes = vec![0xCCu8; 16 + spec.text_padding];
    let text_raw = align_up(text_bytes.len(), FILE_ALIGN);

    let headers_size = FILE_ALIGN;
    let rdata_file = headers_size;
    let text_file = rdata_file + rdata_raw;
    let total = text_file + text_raw;

    // --------------------------------------------------------------- headers
    let mut buf = vec![0u8; total];
    buf[0] = b'M';
    buf[1] = b'Z';
    // e_lfanew: the PE signature sits immediately after the 64-byte DOS header.
    // Everything (DOS + COFF + optional header + 2 section headers) must fit
    // inside the first `headers_size` bytes.
    let pe_at = 0x40usize;
    write_u32(&mut buf, 0x3C, pe_at as u32);
    buf[pe_at..pe_at + 4].copy_from_slice(b"PE\0\0");

    let coff = pe_at + 4;
    write_u16(&mut buf, coff, spec.arch.machine());
    write_u16(&mut buf, coff + 2, 2); // NumberOfSections
    write_u32(&mut buf, coff + 4, spec.timestamp);
    write_u32(&mut buf, coff + 8, 0); // PointerToSymbolTable
    write_u32(&mut buf, coff + 12, 0); // NumberOfSymbols
    write_u16(&mut buf, coff + 16, opt_size as u16);
    let mut characteristics = IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_RELOCS_STRIPPED;
    if is64 {
        characteristics |= IMAGE_FILE_LARGE_ADDRESS_AWARE;
    } else {
        characteristics |= IMAGE_FILE_32BIT_MACHINE;
    }
    if spec.dll {
        characteristics |= IMAGE_FILE_DLL;
    }
    write_u16(&mut buf, coff + 18, characteristics);

    let opt = coff + 20;
    write_u16(&mut buf, opt, if is64 { 0x20B } else { 0x10B }); // Magic
    buf[opt + 2] = 14; // MajorLinkerVersion
    buf[opt + 3] = 0; // MinorLinkerVersion
    write_u32(&mut buf, opt + 4, text_raw as u32); // SizeOfCode
    write_u32(&mut buf, opt + 8, rdata_raw as u32); // SizeOfInitializedData
    write_u32(&mut buf, opt + 12, 0); // SizeOfUninitializedData
    write_u32(&mut buf, opt + 16, TEXT_RVA); // AddressOfEntryPoint
    write_u32(&mut buf, opt + 20, TEXT_RVA); // BaseOfCode
    if is64 {
        write_u64(&mut buf, opt + 24, 0x0000_0001_4000_0000); // ImageBase
        write_u32(&mut buf, opt + 32, SECTION_ALIGN);
        write_u32(&mut buf, opt + 36, FILE_ALIGN as u32);
        write_u16(&mut buf, opt + 40, 10); // MajorOperatingSystemVersion
        write_u16(&mut buf, opt + 42, 0);
        write_u16(&mut buf, opt + 44, 0); // MajorImageVersion
        write_u16(&mut buf, opt + 46, 0);
        write_u16(&mut buf, opt + 48, 6); // MajorSubsystemVersion
        write_u16(&mut buf, opt + 50, 0);
        write_u32(&mut buf, opt + 52, 0); // Win32VersionValue
        write_u32(&mut buf, opt + 56, size_of_image());
        write_u32(&mut buf, opt + 60, headers_size as u32);
        write_u32(&mut buf, opt + 64, 0); // CheckSum
        write_u16(&mut buf, opt + 68, subsystem(spec));
        write_u16(&mut buf, opt + 70, 0x8160); // DllCharacteristics
        write_u64(&mut buf, opt + 72, 0x10_0000); // SizeOfStackReserve
        write_u64(&mut buf, opt + 80, 0x1000);
        write_u64(&mut buf, opt + 88, 0x10_0000); // SizeOfHeapReserve
        write_u64(&mut buf, opt + 96, 0x1000);
        write_u32(&mut buf, opt + 104, 0); // LoaderFlags
        write_u32(&mut buf, opt + 108, 16); // NumberOfRvaAndSizes
        write_dirs(&mut buf, opt + 112, import_dir, com_dir, security_dir);
    } else {
        write_u32(&mut buf, opt + 24, TEXT_RVA); // BaseOfData
        write_u32(&mut buf, opt + 28, 0x0040_0000); // ImageBase
        write_u32(&mut buf, opt + 32, SECTION_ALIGN);
        write_u32(&mut buf, opt + 36, FILE_ALIGN as u32);
        write_u16(&mut buf, opt + 40, 5); // MajorOperatingSystemVersion (XP+)
        write_u16(&mut buf, opt + 42, 0);
        write_u16(&mut buf, opt + 44, 0);
        write_u16(&mut buf, opt + 46, 0);
        write_u16(&mut buf, opt + 48, 5); // MajorSubsystemVersion
        write_u16(&mut buf, opt + 50, 0);
        write_u32(&mut buf, opt + 52, 0);
        write_u32(&mut buf, opt + 56, size_of_image());
        write_u32(&mut buf, opt + 60, headers_size as u32);
        write_u32(&mut buf, opt + 64, 0);
        write_u16(&mut buf, opt + 68, subsystem(spec));
        write_u16(&mut buf, opt + 70, 0x8540); // DllCharacteristics
        write_u32(&mut buf, opt + 72, 0x10_0000); // SizeOfStackReserve
        write_u32(&mut buf, opt + 76, 0x1000);
        write_u32(&mut buf, opt + 80, 0x10_0000);
        write_u32(&mut buf, opt + 84, 0x1000);
        write_u32(&mut buf, opt + 88, 0); // LoaderFlags
        write_u32(&mut buf, opt + 92, 16); // NumberOfRvaAndSizes
        write_dirs(&mut buf, opt + 96, import_dir, com_dir, security_dir);
    }

    // -------------------------------------------------------- section headers
    let sh = opt + opt_size;
    write_section_header(
        &mut buf,
        sh,
        ".rdata",
        rdata_bytes.len() as u32,
        RDATA_RVA,
        rdata_raw as u32,
        rdata_file as u32,
        0x4000_0040, // INITIALIZED_DATA | READ
    );
    write_section_header(
        &mut buf,
        sh + 40,
        ".text",
        text_bytes.len() as u32,
        TEXT_RVA,
        text_raw as u32,
        text_file as u32,
        0x6000_0020, // CODE | EXECUTE | READ
    );

    buf[rdata_file..rdata_file + rdata_bytes.len()].copy_from_slice(&rdata_bytes);
    buf[text_file..text_file + text_bytes.len()].copy_from_slice(&text_bytes);
    buf
}

fn size_of_image() -> u32 {
    ((TEXT_RVA + 0x1000) / SECTION_ALIGN + 1) * SECTION_ALIGN
}

fn subsystem(spec: &PeSpec) -> u16 {
    if spec.gui {
        IMAGE_SUBSYSTEM_WINDOWS_GUI
    } else {
        IMAGE_SUBSYSTEM_WINDOWS_CUI
    }
}

fn write_dirs(
    buf: &mut [u8],
    at: usize,
    import: (u32, u32),
    com: (u32, u32),
    security: (u32, u32),
) {
    for i in 0..16 {
        let entry = at + i * 8;
        let (va, size) = match i {
            1 => import,
            DIR_COM_DESCRIPTOR => com,
            4 => security,
            _ => (0, 0),
        };
        write_u32(buf, entry, va);
        write_u32(buf, entry + 4, size);
    }
}

/// Write one `IMAGE_SECTION_HEADER`.
///
// A section header simply has this many fields; grouping them into a struct
// would move the noise from the signature to every call site.
#[allow(clippy::too_many_arguments)]
fn write_section_header(
    buf: &mut [u8],
    at: usize,
    name: &str,
    virtual_size: u32,
    rva: u32,
    raw_size: u32,
    raw_ptr: u32,
    characteristics: u32,
) {
    let bytes = name.as_bytes();
    buf[at..at + bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
    write_u32(buf, at + 8, virtual_size);
    write_u32(buf, at + 12, rva);
    write_u32(buf, at + 16, raw_size);
    write_u32(buf, at + 20, raw_ptr);
    write_u32(buf, at + 24, 0); // PointerToRelocations
    write_u32(buf, at + 28, 0);
    write_u16(buf, at + 32, 0);
    write_u16(buf, at + 34, 0);
    write_u32(buf, at + 36, characteristics);
}

/// Write an image to `path`, creating parent directories.
pub fn write_exe(path: &Path, spec: &PeSpec) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, synthetic_pe(spec))?;
    Ok(())
}

/// A `.bat` file, the other input type WinDrop accepts.
pub fn write_bat(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)?;
    Ok(())
}

// ------------------------------------------------------------------ helpers

struct SectionWriter {
    base: u32,
    data: Vec<u8>,
}

impl SectionWriter {
    fn new(base: u32) -> Self {
        SectionWriter {
            base,
            data: Vec::new(),
        }
    }

    fn rva_of(&self, offset: usize) -> u32 {
        self.base + offset as u32
    }

    fn reserve(&mut self, len: usize) -> usize {
        let at = self.data.len();
        self.data.resize(at + len, 0);
        at
    }

    fn align(&mut self, boundary: usize) {
        let rem = self.data.len() % boundary;
        if rem != 0 {
            self.reserve(boundary - rem);
        }
    }

    fn patch_u16(&mut self, at: usize, v: u16) {
        self.data[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn patch_u32(&mut self, at: usize, v: u32) {
        self.data[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn patch_ptr(&mut self, at: usize, v: u64, width: usize) {
        if width == 8 {
            self.data[at..at + 8].copy_from_slice(&v.to_le_bytes());
        } else {
            self.patch_u32(at, v as u32);
        }
    }

    fn patch_bytes(&mut self, at: usize, bytes: &[u8]) {
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn finish(self) -> Vec<u8> {
        self.data
    }
}

fn align_up(value: usize, boundary: usize) -> usize {
    if value == 0 {
        return 0;
    }
    value.div_ceil(boundary) * boundary
}

fn write_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn write_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn write_u64(buf: &mut [u8], at: usize, v: u64) {
    buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Deliberately broken inputs, for negative tests.
pub mod corrupt {
    use super::*;

    /// A file with no DOS signature at all.
    pub fn not_a_pe() -> Vec<u8> {
        b"#!/bin/sh\necho 'this is a shell script'\n".to_vec()
    }

    /// Correct `MZ` magic but a truncated body.
    pub fn truncated_mz() -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[0] = b'M';
        v[1] = b'Z';
        v
    }

    /// An empty file.
    pub fn empty() -> Vec<u8> {
        Vec::new()
    }

    /// Writes any byte blob to disk.
    pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Convenience: a plausible-looking but unreadable file.
    pub fn write_bad_exe(path: &Path) -> Result<()> {
        write(path, &not_a_pe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_image_parses_with_goblin() {
        for spec in [
            PeSpec::example_installer(),
            PeSpec::example_d3d12_game(),
            PeSpec::example_console_tool(),
            PeSpec::example_dll(),
            // A managed binary recognised by its imports alone. This shape is
            // accepted by strict full-image parsers.
            PeSpec::default()
                .with_arch(Arch::X86_64)
                .with_imports("mscoree", &["_CorExeMain"]),
        ] {
            let bytes = synthetic_pe(&spec);
            let pe = goblin::pe::PE::parse(&bytes)
                .unwrap_or_else(|e| panic!("goblin rejected our {spec:?}: {e}"));
            assert_eq!(pe.header.coff_header.machine, spec.arch.machine());
        }
    }

    #[test]
    fn minimal_dotnet_stub_is_deliberately_hostile_to_strict_parsers() {
        // The CLR directory is present but its body is a stub. Runners never
        // need to parse it, but a strict full-image parser will refuse the
        // whole file. WinDrop must therefore inspect header fields on its own
        // rather than depending on any single parser succeeding.
        let bytes = synthetic_pe(&PeSpec::example_dotnet_app());
        assert!(goblin::pe::PE::parse(&bytes).is_err());
        // ...while the raw header is still perfectly readable.
        assert_eq!(raw_dir(&bytes, DIR_COM_DESCRIPTOR).1, 72);
        assert_eq!(
            raw_header_field(&bytes, Field::Machine),
            MACHINE_AMD64 as u32
        );
    }

    #[test]
    fn import_table_round_trips_every_function_and_dll() {
        let spec = PeSpec::default()
            .with_imports("KERNEL32", &["CreateFileW", "ReadFile"])
            .with_imports("USER32", &["MessageBoxA"]);
        let bytes = synthetic_pe(&spec);
        let pe = goblin::pe::PE::parse(&bytes).unwrap();

        let mut got: Vec<(String, Vec<String>)> = Vec::new();
        for imp in &pe.imports {
            match got.iter_mut().find(|(d, _)| d == imp.dll) {
                Some((_, fns)) => fns.push(imp.name.to_string()),
                None => got.push((imp.dll.to_string(), vec![imp.name.to_string()])),
            }
        }
        got.sort();
        assert_eq!(
            got,
            vec![
                (
                    "KERNEL32.dll".to_string(),
                    vec!["CreateFileW".into(), "ReadFile".into()]
                ),
                ("USER32.dll".to_string(), vec!["MessageBoxA".into()]),
            ]
        );
    }

    #[test]
    fn libraries_list_is_readable() {
        let bytes = synthetic_pe(&PeSpec::default().with_imports("ntdll", &["NtClose"]));
        let pe = goblin::pe::PE::parse(&bytes).unwrap();
        assert!(pe
            .libraries
            .iter()
            .any(|l| l.eq_ignore_ascii_case("ntdll.dll")));
    }

    #[test]
    fn subsystem_and_dll_flags_are_exact() {
        let mut spec = PeSpec::example_installer();
        spec.gui = false;
        spec.dll = true;
        let bytes = synthetic_pe(&spec);
        let pe = goblin::pe::PE::parse(&bytes).unwrap();
        let optional = pe.header.optional_header.unwrap();
        assert_eq!(
            optional.windows_fields.subsystem,
            IMAGE_SUBSYSTEM_WINDOWS_CUI
        );
        assert!(pe.header.coff_header.characteristics & IMAGE_FILE_DLL != 0);
    }

    #[test]
    fn pe32_and_pe32_plus_magics_differ() {
        let bytes = synthetic_pe(&PeSpec::example_installer().with_arch(Arch::X86));
        let x86 = goblin::pe::PE::parse(&bytes).unwrap();
        assert_eq!(
            x86.header.optional_header.unwrap().standard_fields.magic,
            0x10B
        );

        let bytes = synthetic_pe(&PeSpec::example_console_tool().with_arch(Arch::X86_64));
        let x64 = goblin::pe::PE::parse(&bytes).unwrap();
        assert_eq!(
            x64.header.optional_header.unwrap().standard_fields.magic,
            0x20B
        );
    }

    #[test]
    fn padding_produces_a_large_but_valid_image() {
        let bytes = synthetic_pe(&PeSpec::example_gui_large());
        assert!(bytes.len() > 1024 * 1024, "should look like a real program");
        let pe = goblin::pe::PE::parse(&bytes).unwrap();
        assert_eq!(
            pe.header.optional_header.unwrap().standard_fields.magic,
            0x20B
        );
        assert_eq!(pe.header.coff_header.machine, MACHINE_AMD64);
    }

    #[test]
    fn images_are_byte_for_byte_deterministic() {
        let a = synthetic_pe(&PeSpec::example_installer());
        let b = synthetic_pe(&PeSpec::example_installer());
        assert_eq!(a, b, "fixtures must be reproducible for hashing tests");
    }

    /// Standard PE offsets used by both the fixture writer and the inspector's
    /// parser-free fallback path.
    const COFF_AT: usize = 0x44;
    const OPT_AT: usize = COFF_AT + 20;

    /// Read a data-directory entry straight out of the raw header, involving no
    /// parser at all.
    fn raw_dir(bytes: &[u8], index: usize) -> (u32, u32) {
        let dirs = if is_pe32_plus(bytes) {
            OPT_AT + 0x70
        } else {
            OPT_AT + 0x60
        };
        let at = dirs + index * 8;
        (
            u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
            u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
        )
    }

    fn is_pe32_plus(bytes: &[u8]) -> bool {
        u16::from_le_bytes([bytes[OPT_AT], bytes[OPT_AT + 1]]) == 0x20B
    }

    enum Field {
        Machine,
        Subsystem,
        Characteristics,
    }

    /// Read a COFF/optional-header field without a parser.
    fn raw_header_field(bytes: &[u8], field: Field) -> u32 {
        match field {
            Field::Machine => {
                u16::from_le_bytes(bytes[COFF_AT..COFF_AT + 2].try_into().unwrap()) as u32
            }
            Field::Characteristics => {
                u16::from_le_bytes(bytes[COFF_AT + 18..COFF_AT + 20].try_into().unwrap()) as u32
            }
            Field::Subsystem => {
                // Subsystem sits at optional-header offset 68 for both PE32 and
                // PE32+; the headers diverge only after this point.
                let at = OPT_AT + 68;
                u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as u32
            }
        }
    }

    #[test]
    fn raw_reader_agrees_with_goblin_on_the_import_directory() {
        let bytes = synthetic_pe(&PeSpec::example_installer());
        let pe = goblin::pe::PE::parse(&bytes).unwrap();
        let dirs = pe.header.optional_header.unwrap().data_directories;
        let from_goblin = match dirs.data_directories.get(1) {
            Some(Some((_, d))) => (d.virtual_address, d.size),
            _ => (0, 0),
        };
        assert_eq!(raw_dir(&bytes, 1), from_goblin);
        assert_eq!(raw_dir(&bytes, 1).0, RDATA_RVA);
    }

    #[test]
    fn raw_reader_agrees_with_goblin_on_headers() {
        let bytes = synthetic_pe(&PeSpec::example_dotnet_app());
        assert_eq!(
            raw_header_field(&bytes, Field::Machine),
            MACHINE_AMD64 as u32
        );
        assert_eq!(
            raw_header_field(&bytes, Field::Subsystem),
            IMAGE_SUBSYSTEM_WINDOWS_GUI as u32
        );
        assert_ne!(raw_header_field(&bytes, Field::Characteristics), 0);
        assert!(is_pe32_plus(&bytes));

        let bytes = synthetic_pe(&PeSpec::example_installer());
        assert!(!is_pe32_plus(&bytes));
        assert_eq!(
            raw_header_field(&bytes, Field::Machine),
            MACHINE_I386 as u32
        );
    }

    #[test]
    fn a_dotnet_image_sets_the_clr_directory() {
        let bytes = synthetic_pe(&PeSpec::example_dotnet_app());
        assert!(raw_dir(&bytes, DIR_COM_DESCRIPTOR).1 > 0);

        let bytes = synthetic_pe(&PeSpec::example_console_tool());
        assert_eq!(raw_dir(&bytes, DIR_COM_DESCRIPTOR).1, 0);
    }

    #[test]
    fn file_and_section_alignment_are_valid() {
        let bytes = synthetic_pe(&PeSpec::example_installer());
        assert_eq!(bytes.len() % FILE_ALIGN, 0, "image must be file-aligned");
        // Section raw pointers must be file-aligned too.
        let pe = goblin::pe::PE::parse(&bytes).unwrap();
        for s in &pe.sections {
            let ptr = s.pointer_to_raw_data as usize;
            if ptr != 0 {
                assert_eq!(
                    ptr % FILE_ALIGN,
                    0,
                    "section {:?} not aligned",
                    s.name().unwrap()
                );
            }
        }
    }

    #[test]
    fn corrupt_inputs_are_rejected_by_the_parser() {
        assert!(goblin::pe::PE::parse(&corrupt::not_a_pe()).is_err());
        assert!(goblin::pe::PE::parse(&corrupt::truncated_mz()).is_err());
        assert!(goblin::pe::PE::parse(&corrupt::empty()).is_err());
    }
}

//! Extracting an application's icon.
//!
//! Windows executables embed their icons as RT_GROUP_ICON (type 14) resources.
//! `wrestool` pulls the resource group out, and `icotool` converts it into PNGs
//! at every size the application ships. WinDrop keeps the largest.
//!
//! Icon extraction is strictly best-effort: a missing tool, a protected
//! resource section or an unreadable icon must never stop an install. When it
//! fails the desktop entry falls back to the themed `windrop` icon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{which, CommandSpec};
use crate::{Error, Result};

/// How long the external tools may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// The themed icon name used when nothing can be extracted.
pub const FALLBACK_ICON_NAME: &str = "windrop";

/// Locates and runs the icon tools.
#[derive(Debug, Clone, Default)]
pub struct IconExtractor {
    wrestool: Option<PathBuf>,
    icotool: Option<PathBuf>,
}

impl IconExtractor {
    /// Use the tools found on `$PATH`, if any.
    pub fn from_system() -> Self {
        IconExtractor {
            wrestool: which("wrestool"),
            icotool: which("icotool"),
        }
    }

    /// Inject explicit tool paths, for tests and for unusual installations.
    pub fn with_tools(wrestool: Option<PathBuf>, icotool: Option<PathBuf>) -> Self {
        IconExtractor { wrestool, icotool }
    }

    /// Both tools present. Without both, extraction is skipped.
    pub fn available(&self) -> bool {
        self.wrestool.is_some() && self.icotool.is_some()
    }

    /// The Arch package that provides the tools, for the setup guide.
    pub fn missing_tool_hint(&self) -> Option<&'static str> {
        if self.available() {
            None
        } else {
            Some("Install icoutils (sudo pacman -S icoutils) to extract application icons.")
        }
    }

    /// Extract the largest icon from `exe` into `out_dir`, named `icon.png`.
    ///
    /// Returns `Ok(None)` whenever an icon simply could not be produced. Only a
    /// filesystem failure is an error, because that affects the whole install.
    pub fn extract(&self, exe: &Path, out_dir: &Path) -> Result<Option<PathBuf>> {
        if !self.available() {
            tracing::debug!("icon tools are unavailable; using the themed fallback icon");
            return Ok(None);
        }
        let (Some(wrestool), Some(icotool)) = (&self.wrestool, &self.icotool) else {
            return Ok(None);
        };

        std::fs::create_dir_all(out_dir)?;

        // 1. Dump the icon resource group. wrestool writes `<name>_14_1.ico`.
        let dump = CommandSpec::new(wrestool)
            .args(["-x", "-t", "14"])
            .arg(exe)
            .args(["-o"])
            .arg(out_dir);
        let out = dump.run_capture(TOOL_TIMEOUT)?;
        if !out.success() {
            tracing::warn!(
                error = %out.stderr.trim(),
                "wrestool could not read an icon from the executable"
            );
            return Ok(None);
        }

        let Some(ico) = find_first_with_extension(out_dir, "ico") else {
            tracing::debug!("the executable contains no icon resources");
            return Ok(None);
        };

        // 2. Convert every embedded size to PNG.
        let png_dir = out_dir.join("png");
        std::fs::create_dir_all(&png_dir)?;
        let convert = CommandSpec::new(icotool)
            .arg("-x")
            .args(["-o"])
            .arg(&png_dir)
            .arg(&ico);
        let out = convert.run_capture(TOOL_TIMEOUT)?;
        if !out.success() {
            tracing::warn!(
                error = %out.stderr.trim(),
                "icotool could not convert the icon"
            );
            return Ok(None);
        }

        // 3. Keep the largest size and clean up the intermediates.
        let Some(best) = pick_largest_png(&png_dir) else {
            tracing::debug!("icotool produced no PNG files");
            return Ok(None);
        };
        let target = out_dir.join("icon.png");
        std::fs::copy(&best, &target)?;

        let _ = std::fs::remove_file(&ico);
        let _ = std::fs::remove_dir_all(&png_dir);
        tracing::info!(icon = %target.display(), "extracted application icon");
        Ok(Some(target))
    }
}

/// The first file in `dir` with the given extension, case-insensitively.
pub fn find_first_with_extension(dir: &Path, extension: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .map(|e| e.eq_ignore_ascii_case(extension))
                    .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Pick the PNG with the largest area.
///
/// `icotool` names its output `<source>_<index>_<width>x<height>x<depth>.png`, so
/// the size is read from the filename. Files whose name does not follow that
/// pattern are still eligible, ranked by file length as a proxy.
pub fn pick_largest_png(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_png = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("png"))
            .unwrap_or(false);
        if !is_png {
            continue;
        }
        let area = png_area_from_name(&path)
            .unwrap_or_else(|| entry.metadata().map(|m| m.len()).unwrap_or(0));
        match &best {
            Some((best_area, _)) if *best_area >= area => {}
            _ => best = Some((area, path)),
        }
    }
    best.map(|(_, path)| path)
}

/// Parse `123x45` or `123x45x32` out of a filename.
fn png_area_from_name(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_string_lossy().to_string();
    for (start, _) in name.char_indices().filter(|(_, c)| c.is_ascii_digit()) {
        let tail = &name[start..];
        let mut parts = tail.split('x');
        let (Some(w), Some(h)) = (parts.next(), parts.next()) else {
            continue;
        };
        // `w` may have a leading index glued on, e.g. `app_1_32x32x32.png`.
        let w_digits: String = w.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
        let w_digits: String = w_digits.chars().rev().collect();
        let h_digits: String = h.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let (Ok(w), Ok(h)) = (w_digits.parse::<u64>(), h_digits.parse::<u64>()) {
            if w > 0 && h > 0 {
                return Some(w * h);
            }
        }
    }
    None
}

/// Where a per-application icon lives.
pub fn icon_path(app_dir: &Path) -> PathBuf {
    app_dir.join("icon.png")
}

/// The icon reference for a `.desktop` file: a real path when one was
/// extracted, otherwise a themed icon name.
pub fn desktop_icon_value(app_dir: &Path) -> String {
    let path = icon_path(app_dir);
    if path.is_file() {
        path.to_string_lossy().to_string()
    } else {
        FALLBACK_ICON_NAME.to_string()
    }
}

/// Copy a stand-in icon into place. Used by tests and by the demo application.
pub fn write_placeholder_icon(app_dir: &Path, png_bytes: &[u8]) -> Result<PathBuf> {
    let path = icon_path(app_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, png_bytes)?;
    Ok(path)
}

/// Turn an extraction failure into the log line the user sees.
pub fn describe_failure(error: &Error) -> String {
    format!("icon extraction failed ({error}); the themed icon will be used instead")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `wrestool` that writes an `.ico` into the `-o` directory.
    const FAKE_WRESTOOL: &str = r#"#!/bin/sh
out="."
while [ $# -gt 0 ]; do
  case "$1" in -o) shift; out="$1" ;; esac
  shift
done
mkdir -p "$out"
printf 'icon resource' > "$out/app.exe_14_1.ico"
exit 0
"#;

    /// A fake `icotool` that emits two sizes into the `-o` directory.
    const FAKE_ICOTOOL: &str = r#"#!/bin/sh
out="."
while [ $# -gt 0 ]; do
  case "$1" in -o) shift; out="$1" ;; esac
  shift
done
mkdir -p "$out"
printf 'small'  > "$out/app.exe_1_16x16x32.png"
printf 'large!' > "$out/app.exe_2_48x48x32.png"
exit 0
"#;

    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fake_tools(dir: &Path) -> IconExtractor {
        let wrestool = dir.join("wrestool");
        let icotool = dir.join("icotool");
        write_script(&wrestool, FAKE_WRESTOOL);
        write_script(&icotool, FAKE_ICOTOOL);
        IconExtractor::with_tools(Some(wrestool), Some(icotool))
    }

    #[test]
    fn an_icon_is_extracted_and_the_largest_size_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("app.exe");
        std::fs::write(&exe, b"not a real exe, only the tools care").unwrap();

        let out = tmp.path().join("icon");
        let extractor = fake_tools(tmp.path());
        let icon = extractor.extract(&exe, &out).unwrap().expect("an icon");

        assert_eq!(icon, out.join("icon.png"));
        assert_eq!(std::fs::read(&icon).unwrap(), b"large!");
        // Intermediates are cleaned up.
        assert!(!out.join("png").exists());
        assert!(!out.join("app.exe_14_1.ico").exists());
    }

    #[test]
    fn without_the_tools_extraction_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("app.exe");
        std::fs::write(&exe, b"x").unwrap();

        let extractor = IconExtractor::with_tools(None, None);
        assert!(!extractor.available());
        assert!(extractor
            .extract(&exe, &tmp.path().join("icon"))
            .unwrap()
            .is_none());
        assert!(extractor.missing_tool_hint().unwrap().contains("icoutils"));
    }

    #[test]
    fn one_missing_tool_disables_extraction() {
        let tmp = tempfile::tempdir().unwrap();
        let extractor = IconExtractor::with_tools(Some(PathBuf::from("/bin/true")), None);
        assert!(!extractor.available());
        assert!(extractor
            .extract(&tmp.path().join("a.exe"), &tmp.path().join("o"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_failing_wrestool_does_not_fail_the_install() {
        let tmp = tempfile::tempdir().unwrap();
        let broken = tmp.path().join("wrestool");
        write_script(&broken, "#!/bin/sh\necho 'no resources' >&2\nexit 1\n");
        let extractor = IconExtractor::with_tools(Some(broken), Some(PathBuf::from("/bin/true")));

        let exe = tmp.path().join("app.exe");
        std::fs::write(&exe, b"x").unwrap();
        assert!(extractor
            .extract(&exe, &tmp.path().join("icon"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn an_executable_without_icons_yields_none() {
        let tmp = tempfile::tempdir().unwrap();
        let wrestool = tmp.path().join("wrestool");
        write_script(&wrestool, "#!/bin/sh\nexit 0\n"); // writes nothing
        let extractor = IconExtractor::with_tools(Some(wrestool), Some(PathBuf::from("/bin/true")));

        let exe = tmp.path().join("app.exe");
        std::fs::write(&exe, b"x").unwrap();
        assert!(extractor.extract(&exe, tmp.path()).unwrap().is_none());
    }

    #[test]
    fn a_failing_icotool_yields_none() {
        let tmp = tempfile::tempdir().unwrap();
        let icotool = tmp.path().join("icotool");
        write_script(&icotool, "#!/bin/sh\nexit 4\n");
        let extractor = IconExtractor::with_tools(Some(PathBuf::from("/bin/true")), Some(icotool));
        let exe = tmp.path().join("app.exe");
        std::fs::write(&exe, b"x").unwrap();
        assert!(extractor
            .extract(&exe, &tmp.path().join("icon"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn largest_png_is_chosen_by_area_not_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["z_1_256x256x32.png", "a_2_16x16x32.png", "m_3_48x48x32.png"] {
            std::fs::write(tmp.path().join(name), b"x").unwrap();
        }
        let best = pick_largest_png(tmp.path()).unwrap();
        assert_eq!(best.file_name().unwrap(), "z_1_256x256x32.png");
    }

    #[test]
    fn non_png_files_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("readme.txt"), vec![0u8; 5000]).unwrap();
        std::fs::write(tmp.path().join("icon_1_16x16x32.png"), b"x").unwrap();
        assert_eq!(
            pick_largest_png(tmp.path()).unwrap().file_name().unwrap(),
            "icon_1_16x16x32.png"
        );
    }

    #[test]
    fn a_directory_without_pngs_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(pick_largest_png(tmp.path()).is_none());
        assert!(pick_largest_png(&tmp.path().join("missing")).is_none());
    }

    #[test]
    fn sizes_are_parsed_from_various_filename_shapes() {
        assert_eq!(
            png_area_from_name(Path::new("a_1_32x32x32.png")),
            Some(1024)
        );
        assert_eq!(png_area_from_name(Path::new("icon_48x48.png")), Some(2304));
        assert_eq!(png_area_from_name(Path::new("weird.png")), None);
        assert_eq!(png_area_from_name(Path::new("x_0x0.png")), None);
    }

    #[test]
    fn the_desktop_value_falls_back_to_the_themed_name() {
        let tmp = tempfile::tempdir().unwrap();
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        assert_eq!(desktop_icon_value(&app_dir), "windrop");

        write_placeholder_icon(&app_dir, b"\x89PNG").unwrap();
        assert_eq!(
            desktop_icon_value(&app_dir),
            icon_path(&app_dir).to_string_lossy()
        );
    }

    #[test]
    fn the_first_matching_extension_is_returned_deterministically() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b.ico"), b"b").unwrap();
        std::fs::write(tmp.path().join("a.ico"), b"a").unwrap();
        std::fs::write(tmp.path().join("c.png"), b"c").unwrap();
        assert_eq!(
            find_first_with_extension(tmp.path(), "ICO")
                .unwrap()
                .file_name()
                .unwrap(),
            "a.ico"
        );
        assert_eq!(
            find_first_with_extension(tmp.path(), "png")
                .unwrap()
                .file_name()
                .unwrap(),
            "c.png"
        );
        assert!(find_first_with_extension(tmp.path(), "svg").is_none());
    }
}

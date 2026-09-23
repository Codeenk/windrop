//! Desktop integration: the `.desktop` entry that puts an application in the
//! user's menu.
//!
//! ## Why the `Exec` line calls WinDrop rather than Wine
//!
//! It would be possible to bake the whole Wine invocation into `Exec`, but that
//! string has to survive the Desktop Entry specification's quoting rules with a
//! Windows path full of backslashes inside it, and it would freeze the Wine
//! version and environment into a file that lives outside WinDrop's data
//! directory.
//!
//! Instead the entry is a thin launcher:
//!
//! ```ini
//! Exec=windrop launch notepad
//! ```
//!
//! and WinDrop resolves the recorded profile, Wine build, DXVK settings and
//! sandbox at launch time. The `.desktop` file stays valid across Wine upgrades
//! and when the user changes settings, and there is exactly one source of truth
//! for how an application starts.

use std::path::{Path, PathBuf};

use crate::compat::profile::GraphicsApi;
use crate::paths::{desktop_entry_id, Paths};
use crate::process::{which, CommandSpec};
use crate::{Error, Result};

/// The command that `.desktop` entries call back into.
pub const LAUNCHER_BINARY: &str = "windrop";

/// Every field of a desktop entry WinDrop writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// WinDrop's application id.
    pub app_id: String,
    /// Display name, already cleaned of installer noise.
    pub name: String,
    pub comment: String,
    /// A path to an extracted PNG, or a themed icon name.
    pub icon: String,
    pub categories: Vec<String>,
    pub terminal: bool,
    /// Set so window managers can associate the window with this entry.
    pub startup_wm_class: Option<String>,
    /// The data directory to pass back to the launcher.
    ///
    /// `None` means "the default", which is what almost every installation
    /// wants. It is only set when WinDrop lives somewhere else, because a menu
    /// entry is started by the desktop environment rather than by a shell — it
    /// inherits no `WINDROP_DATA_DIR` and would otherwise look for the
    /// application in a directory that does not contain it.
    pub data_dir: Option<PathBuf>,
}

impl DesktopEntry {
    pub fn new(
        app_id: impl Into<String>,
        name: impl Into<String>,
        icon: impl Into<String>,
    ) -> Self {
        DesktopEntry {
            app_id: app_id.into(),
            name: name.into(),
            comment: "Windows application managed by WinDrop".to_string(),
            icon: icon.into(),
            categories: vec!["Utility".to_string()],
            terminal: false,
            startup_wm_class: None,
            data_dir: None,
        }
    }

    /// Record the data directory, when it is not the default one.
    pub fn with_data_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.data_dir = dir;
        self
    }

    pub fn with_categories(mut self, categories: Vec<String>) -> Self {
        self.categories = categories;
        self
    }

    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = comment.into();
        self
    }

    /// The `Exec` value, quoted for the desktop entry grammar.
    ///
    /// Each argument is quoted separately, because the desktop entry grammar
    /// splits on whitespace *outside* quotes: quoting `launch <id>` as one
    /// token would hand the launcher a single argument reading
    /// `launch notepadpp` instead of a subcommand and an id.
    pub fn exec_value(&self) -> String {
        let mut parts = vec![desktop_exec_quote(LAUNCHER_BINARY).to_string()];
        if let Some(dir) = &self.data_dir {
            parts.push("--data-dir".to_string());
            parts.push(desktop_exec_quote(&dir.to_string_lossy()));
        }
        parts.push("launch".to_string());
        parts.push(desktop_exec_quote(&self.app_id));
        parts.join(" ")
    }

    /// True when this entry belongs to `app_id`.
    pub fn matches(&self, app_id: &str) -> bool {
        self.app_id == app_id
    }

    fn validate(&self) -> Result<()> {
        if self.app_id.trim().is_empty() {
            return Err(Error::Config {
                field: "desktop.app_id".into(),
                reason: "must not be empty".into(),
            });
        }
        // The id becomes a filename and part of a command line, so it must not
        // be able to escape either.
        if !self
            .app_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(Error::Config {
                field: "desktop.app_id".into(),
                reason: format!(
                    "'{}' contains characters that are not allowed in a filename",
                    self.app_id
                ),
            });
        }
        if self.name.trim().is_empty() {
            return Err(Error::Config {
                field: "desktop.name".into(),
                reason: "must not be empty".into(),
            });
        }
        // A newline in a value would inject arbitrary keys into the entry.
        let data_dir = self
            .data_dir
            .as_ref()
            .map(|d| d.to_string_lossy().to_string());
        for (field, value) in [
            ("name", &self.name),
            ("comment", &self.comment),
            ("icon", &self.icon),
            ("data_dir", data_dir.as_ref().unwrap_or(&String::new())),
        ] {
            if value.contains(['\n', '\r']) {
                return Err(Error::Config {
                    field: format!("desktop.{field}"),
                    reason: "must not contain line breaks".into(),
                });
            }
        }
        Ok(())
    }

    /// Render the file contents.
    pub fn render(&self) -> Result<String> {
        self.validate()?;
        let categories = if self.categories.is_empty() {
            "Utility;".to_string()
        } else {
            self.categories
                .iter()
                .map(|c| format!("{};", c.trim_end_matches(';')))
                .collect::<Vec<_>>()
                .join("")
        };

        let mut out = String::new();
        out.push_str("[Desktop Entry]\n");
        out.push_str("Version=1.0\n");
        out.push_str("Type=Application\n");
        out.push_str(&format!("Name={}\n", sanitize_value(&self.name)));
        out.push_str(&format!("Comment={}\n", sanitize_value(&self.comment)));
        out.push_str(&format!("Exec={}\n", self.exec_value()));
        out.push_str(&format!("TryExec={}\n", LAUNCHER_BINARY));
        out.push_str(&format!("Icon={}\n", sanitize_value(&self.icon)));
        out.push_str(&format!(
            "Terminal={}\n",
            if self.terminal { "true" } else { "false" }
        ));
        out.push_str(&format!("Categories={categories}\n"));
        out.push_str("StartupNotify=true\n");
        if let Some(class) = &self.startup_wm_class {
            out.push_str(&format!("StartupWMClass={}\n", sanitize_value(class)));
        }
        out.push_str(&format!("X-WinDrop-AppId={}\n", self.app_id));
        Ok(out)
    }

    /// Write the entry into a directory of desktop files.
    pub fn write_to_dir(&self, dir: &Path) -> Result<PathBuf> {
        let contents = self.render()?;
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.desktop", desktop_entry_id(&self.app_id)));
        std::fs::write(&path, contents)?;
        tracing::info!(entry = %path.display(), "wrote desktop entry");
        Ok(path)
    }
}

/// Replace characters that are not meaningful in a display value.
///
/// Tabs become spaces and control characters are dropped, so a hostile
/// application name cannot introduce new keys or break parsing.
pub fn sanitize_value(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    cleaned.trim().to_string()
}

/// Quote a single `Exec` token per the Desktop Entry specification.
///
/// Inside double quotes, backslash must be escaped — which matters because
/// Windows paths are full of them.
pub fn desktop_exec_quote(token: &str) -> String {
    let safe = !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c));
    if safe {
        return token.to_string();
    }
    let mut quoted = String::from("\"");
    for ch in token.chars() {
        if matches!(ch, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

/// Categories for an application, based on what the inspector learned.
pub fn categories_for(graphics: Option<GraphicsApi>) -> Vec<String> {
    match graphics {
        // Anything that renders Direct3D is almost certainly a game.
        Some(GraphicsApi::D3D9) | Some(GraphicsApi::D3D11) | Some(GraphicsApi::D3D12) => {
            vec!["Game".to_string()]
        }
        _ => vec!["Utility".to_string()],
    }
}

/// Install an entry into the user's applications directory.
pub fn install(paths: &Paths, entry: &DesktopEntry) -> Result<PathBuf> {
    let entry = entry.clone().with_data_dir(paths.non_default_data_dir());
    let path = entry.write_to_dir(paths.applications_dir())?;
    refresh_database(paths.applications_dir())?;
    Ok(path)
}

/// Remove an application's entry, if present.
///
/// Returns whether a file was actually removed, so removal can be idempotent
/// without silently pretending to have done something.
pub fn uninstall(paths: &Paths, app_id: &str) -> Result<bool> {
    let path = paths.desktop_file_for(app_id);
    let removed = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(Error::Io(e)),
    };
    refresh_database(paths.applications_dir())?;
    Ok(removed)
}

/// Rebuild the menu database so the entry appears immediately.
///
/// Returns whether the helper ran. A missing helper is not an error: menus are
/// rebuilt by the desktop environment at login anyway.
pub fn refresh_database(apps_dir: &Path) -> Result<bool> {
    let Some(tool) = which("update-desktop-database") else {
        tracing::debug!(
            "update-desktop-database is not installed; the menu will refresh on next login"
        );
        return Ok(false);
    };
    let spec = CommandSpec::new(tool).arg(apps_dir);
    match spec.run_capture(std::time::Duration::from_secs(30)) {
        Ok(out) if out.success() => Ok(true),
        Ok(out) => {
            tracing::debug!(
                code = out.code(),
                stderr = %out.stderr.trim(),
                "update-desktop-database reported a problem"
            );
            Ok(false)
        }
        Err(e) => {
            tracing::debug!(error = %e, "could not run update-desktop-database");
            Ok(false)
        }
    }
}

/// Parse the values out of a rendered entry, for verification and tests.
pub fn parse_entry(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::which;

    fn entry() -> DesktopEntry {
        DesktopEntry::new("notepadpp", "Notepad++", "/data/apps/notepadpp/icon.png")
    }

    #[test]
    fn a_rendered_entry_has_every_required_key() {
        let text = entry().render().unwrap();
        let fields = parse_entry(&text);

        assert!(text.starts_with("[Desktop Entry]\n"));
        assert_eq!(fields["Type"], "Application");
        assert_eq!(fields["Name"], "Notepad++");
        assert_eq!(fields["Exec"], "windrop launch notepadpp");
        assert_eq!(fields["TryExec"], "windrop");
        assert_eq!(fields["Icon"], "/data/apps/notepadpp/icon.png");
        assert_eq!(fields["Terminal"], "false");
        assert_eq!(fields["Categories"], "Utility;");
        assert_eq!(fields["X-WinDrop-AppId"], "notepadpp");
        assert_eq!(fields["StartupNotify"], "true");
    }

    #[test]
    fn the_exec_line_calls_back_into_windrop_rather_than_baking_in_wine() {
        let text = entry().render().unwrap();
        let fields = parse_entry(&text);
        assert!(fields["Exec"].starts_with("windrop"));
        assert!(
            !text.contains("WINEPREFIX"),
            "no Wine details may leak into the entry"
        );
        assert!(
            !text.contains("wine"),
            "no Wine details may leak into the entry"
        );
    }

    /// Split an `Exec` value the way a desktop-environment launcher does:
    /// whitespace separates arguments except inside double quotes.
    fn split_exec(value: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        let mut escaped = false;
        for ch in value.chars() {
            if escaped {
                current.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            args.push(current);
        }
        args
    }

    #[test]
    fn the_exec_line_splits_into_a_subcommand_and_an_id() {
        // The launcher must receive three arguments, not two with one of them
        // reading "launch notepadpp".
        let fields = parse_entry(&entry().render().unwrap());
        assert_eq!(
            split_exec(&fields["Exec"]),
            vec!["windrop", "launch", "notepadpp"]
        );
    }

    #[test]
    fn every_exec_token_is_quoted_independently() {
        // Sanity-check the quoting helper against the splitting rules above, so
        // the two cannot drift apart.
        for token in ["windrop", "launch notepad", "a b", "C:\\Program Files"] {
            assert_eq!(
                split_exec(&desktop_exec_quote(token)),
                vec![token.to_string()]
            );
        }
    }

    #[test]
    fn categories_follow_the_graphics_api() {
        assert_eq!(
            categories_for(Some(GraphicsApi::D3D11)),
            vec!["Game".to_string()]
        );
        assert_eq!(
            categories_for(Some(GraphicsApi::D3D12)),
            vec!["Game".to_string()]
        );
        assert_eq!(
            categories_for(Some(GraphicsApi::D3D9)),
            vec!["Game".to_string()]
        );
        assert_eq!(categories_for(None), vec!["Utility".to_string()]);
        assert_eq!(
            categories_for(Some(GraphicsApi::Vulkan)),
            vec!["Utility".to_string()]
        );
    }

    #[test]
    fn multiple_categories_are_semicolon_terminated_without_duplicating() {
        let e = entry().with_categories(vec!["Game;".into(), "Utility".into()]);
        let fields = parse_entry(&e.render().unwrap());
        assert_eq!(fields["Categories"], "Game;Utility;");
    }

    #[test]
    fn an_empty_category_list_falls_back_to_utility() {
        let e = entry().with_categories(vec![]);
        assert_eq!(parse_entry(&e.render().unwrap())["Categories"], "Utility;");
    }

    #[test]
    fn a_name_containing_a_line_break_is_rejected() {
        let mut e = entry();
        e.name = "Innocent\nExec=/bin/sh -c 'rm -rf /'".into();
        match e.render() {
            Err(Error::Config { field, .. }) => assert_eq!(field, "desktop.name"),
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn control_characters_are_stripped_from_display_values() {
        assert_eq!(sanitize_value("Tab\there"), "Tab here");
        assert_eq!(sanitize_value("bell\u{7}and"), "belland");
        assert_eq!(sanitize_value("  padded  "), "padded");
        let mut e = entry();
        e.name = "Weird\u{0}Name".into();
        let fields = parse_entry(&e.render().unwrap());
        assert_eq!(fields["Name"], "WeirdName");
    }

    #[test]
    fn an_app_id_may_not_escape_the_applications_directory() {
        for bad in ["../../etc/passwd", "a/b", "a b", "app;rm -rf /", ""] {
            let e = DesktopEntry::new(bad, "Name", "windrop");
            assert!(
                matches!(e.render(), Err(Error::Config { .. })),
                "'{bad}' should be refused"
            );
        }
    }

    #[test]
    fn an_empty_name_is_refused() {
        let e = DesktopEntry::new("app", "   ", "windrop");
        assert!(matches!(e.render(), Err(Error::Config { .. })));
    }

    #[test]
    fn exec_quoting_escapes_backslashes_and_quotes() {
        assert_eq!(desktop_exec_quote("windrop"), "windrop");
        assert_eq!(desktop_exec_quote("simple-name_1.2"), "simple-name_1.2");
        assert_eq!(desktop_exec_quote("launch notepad"), "\"launch notepad\"");
        assert_eq!(
            desktop_exec_quote(r"C:\Program Files\App"),
            r#""C:\\Program Files\\App""#
        );
        assert_eq!(desktop_exec_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(desktop_exec_quote("$HOME"), "\"\\$HOME\"");
        assert_eq!(desktop_exec_quote(""), "\"\"");
    }

    #[test]
    fn writing_and_removing_an_entry_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::isolated(tmp.path());
        paths.ensure().unwrap();

        let path = install(&paths, &entry()).unwrap();
        assert_eq!(path, paths.desktop_file_for("notepadpp"));
        assert!(path.is_file());

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("Name=Notepad++"));
        assert_eq!(parse_entry(&written)["X-WinDrop-AppId"], "notepadpp");

        assert!(uninstall(&paths, "notepadpp").unwrap());
        assert!(!path.exists());
        // Idempotent: a second removal reports that there was nothing to do.
        assert!(!uninstall(&paths, "notepadpp").unwrap());
    }

    #[test]
    fn installing_twice_overwrites_rather_than_duplicating() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::isolated(tmp.path());
        paths.ensure().unwrap();

        install(&paths, &entry()).unwrap();
        let renamed = DesktopEntry::new("notepadpp", "Notepad++ 8.6", "windrop");
        install(&paths, &renamed).unwrap();

        // Count entries, not files: `update-desktop-database` also maintains a
        // `mimeinfo.cache` in the same directory, which is expected.
        let entries: Vec<_> = std::fs::read_dir(paths.applications_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".desktop"))
            .collect();
        assert_eq!(
            entries,
            vec!["org.windrop.WinDrop.notepadpp.desktop".to_string()]
        );
        assert!(std::fs::read_to_string(paths.desktop_file_for("notepadpp"))
            .unwrap()
            .contains("Name=Notepad++ 8.6"));
    }

    #[test]
    fn startup_wm_class_is_only_written_when_known() {
        assert!(!entry().render().unwrap().contains("StartupWMClass"));
        let e = DesktopEntry {
            startup_wm_class: Some("Notepad".into()),
            ..entry()
        };
        assert_eq!(
            parse_entry(&e.render().unwrap())["StartupWMClass"],
            "Notepad"
        );
    }

    #[test]
    fn the_database_refresh_reports_whether_the_helper_ran() {
        let tmp = tempfile::tempdir().unwrap();
        let ran = refresh_database(tmp.path()).unwrap();
        assert_eq!(
            ran,
            which("update-desktop-database").is_some(),
            "the reported result must match the host"
        );
    }

    #[test]
    fn matching_compares_the_application_id() {
        let e = entry();
        assert!(e.matches("notepadpp"));
        assert!(!e.matches("somethingelse"));
    }

    #[test]
    fn parsing_ignores_the_header_and_comments() {
        let text = "[Desktop Entry]\n# a comment\nName=X\n\nKey=Value\n";
        let fields = parse_entry(text);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields["Name"], "X");
        assert_eq!(fields["Key"], "Value");
    }
}

//! The WinDrop window.
//!
//! Deliberately small: a drop zone, the list of installed applications, a setup
//! banner when something is missing, and a status area. Everything Wine-specific
//! — prefixes, DLL overrides, DXVK versions, sandbox flags — is hidden, because
//! none of it is a decision anyone should have to make in order to run a program
//! they already own.
//!
//! The window owns no state that matters. Every list is re-read from disk, every
//! setting is written through [`Config`], and the on-disk layout is the single
//! source of truth — so the window, the command line and a text editor open on
//! `metadata.json` can never disagree.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use windrop_core::compat::pe::PeInspection;
use windrop_core::config::{Config, PerformanceMode, SandboxMode, WineVariant};
use windrop_core::doctor::{Diagnostics, Necessity};
use windrop_core::manager::human_bytes;
use windrop_core::manager::metadata::InstalledApp;
use windrop_core::paths::Paths;
use windrop_core::text;
use windrop_core::updater::ProfileUpdate;

use crate::worker::{self, Message, Task};

/// How often the main loop drains the worker channel.
///
/// Fast enough that a progress line appears at once, slow enough that an idle
/// window is genuinely idle rather than waking sixty times a second.
const TICK: Duration = Duration::from_millis(80);

/// The pieces of WinDrop the window talks to.
pub struct Ui {
    pub paths: Paths,
    pub config: RefCell<Config>,
    pub sender: Sender<Message>,
}

impl Ui {
    fn task(&self, task: Task) {
        worker::spawn(
            self.sender.clone(),
            self.paths.clone(),
            self.config.borrow().clone(),
            task,
        );
    }
}

/// The whole window.
pub struct Window {
    ui: Rc<Ui>,
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    list: gtk::ListBox,
    banner: gtk::Revealer,
    banner_text: gtk::Label,
    banner_detail: gtk::Label,
    banner_command: gtk::Label,
    banner_commands: gtk::Box,
    status: Status,
    spinner: gtk::Spinner,
    /// How many operations are in flight; the spinner stops when it reaches zero.
    busy: RefCell<usize>,
    /// The file currently being inspected or confirmed.
    current: RefCell<Option<PathBuf>>,
    /// Files dropped while something else was in progress.
    queued: RefCell<VecDeque<PathBuf>>,
    /// The last inspection, so the confirmation can describe the file.
    inspection: RefCell<Option<PeInspection>>,
    /// The last diagnostics run, so the report is instant to open.
    diagnostics: RefCell<Option<Diagnostics>>,
    /// The open diagnostics pane, if any, so it can update itself.
    diagnostics_view: RefCell<Option<gtk::TextView>>,
    /// Updated profiles found by the last check.
    updates: RefCell<Vec<ProfileUpdate>>,
    icon_hint: Option<&'static str>,
}

impl Window {
    pub fn new(
        application: &gtk::Application,
        paths: Paths,
        config: Config,
        sender: Sender<Message>,
    ) -> Rc<Self> {
        let config_for_ui = config.clone();
        let ui = Rc::new(Ui {
            paths,
            config: RefCell::new(config_for_ui),
            sender,
        });

        let icon_hint = windrop_core::icons::IconExtractor::from_system().missing_tool_hint();

        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title("WinDrop")
            .default_width(920)
            .default_height(640)
            .build();

        // --------------------------------------------------------------- banner
        let banner_text = gtk::Label::builder().xalign(0.0).wrap(true).build();
        banner_text.add_css_class("heading");
        let banner_detail = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let banner_command = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        banner_command.add_css_class("monospace");
        let copy_command = gtk::Button::with_label("Copy the command");
        let recheck = gtk::Button::with_label("Check again");
        let banner_commands = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        banner_commands.append(&copy_command);
        banner_commands.append(&recheck);

        let banner_content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        banner_content.set_margin_top(12);
        banner_content.set_margin_bottom(12);
        banner_content.set_margin_start(12);
        banner_content.set_margin_end(12);
        banner_content.append(&banner_text);
        banner_content.append(&banner_detail);
        banner_content.append(&banner_command);
        banner_content.append(&banner_commands);
        banner_content.add_css_class("card");

        let banner = gtk::Revealer::builder()
            .child(&banner_content)
            .reveal_child(false)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();

        // -------------------------------------------------------------- drop zone
        let drop_icon = gtk::Image::from_icon_name("application-x-executable");
        drop_icon.set_pixel_size(96);
        drop_icon.add_css_class("dim-label");
        let drop_title = gtk::Label::new(Some("Drop a Windows program here"));
        drop_title.add_css_class("title-1");
        let drop_hint = gtk::Label::new(Some(
            "Or click Choose a file. WinDrop builds a private environment for it, \
             runs the installer, and adds it to your menu.",
        ));
        drop_hint.add_css_class("dim-label");
        drop_hint.set_wrap(true);
        drop_hint.set_justify(gtk::Justification::Center);
        drop_hint.set_max_width_chars(56);
        let choose = gtk::Button::with_label("Choose a file…");
        choose.add_css_class("suggested-action");
        choose.set_halign(gtk::Align::Center);

        let drop_box = gtk::Box::new(gtk::Orientation::Vertical, 14);
        drop_box.set_valign(gtk::Align::Center);
        drop_box.set_halign(gtk::Align::Center);
        drop_box.set_vexpand(true);
        drop_box.append(&drop_icon);
        drop_box.append(&drop_title);
        drop_box.append(&drop_hint);
        drop_box.append(&choose);

        // ------------------------------------------------------------- app list
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        list.set_margin_top(12);
        list.set_margin_bottom(12);
        list.set_margin_start(12);
        list.set_margin_end(12);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        stack.add_named(&drop_box, Some("empty"));
        stack.add_named(&scroller, Some("apps"));

        // --------------------------------------------------------------- status
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        let status = Status::new();
        let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        status_bar.set_margin_top(8);
        status_bar.set_margin_bottom(8);
        status_bar.set_margin_start(12);
        status_bar.set_margin_end(12);
        status_bar.append(&spinner);
        status_bar.append(&status.label);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&banner);
        root.append(&stack);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&status_bar);

        // ------------------------------------------------------------------ menu
        let menu = gio::Menu::new();
        menu.append(
            Some("Check for updated profiles"),
            Some("win.check-updates"),
        );
        menu.append(Some("Settings"), Some("win.settings"));
        menu.append(Some("Diagnostics"), Some("win.diagnostics"));
        menu.append(Some("Quit"), Some("win.quit"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build();
        let header = gtk::HeaderBar::new();
        header.pack_end(&menu_button);

        window.set_titlebar(Some(&header));
        window.set_child(Some(&root));

        let this = Rc::new(Window {
            ui,
            window: window.clone(),
            stack,
            list,
            banner,
            banner_text,
            banner_detail,
            banner_command,
            banner_commands,
            status,
            spinner,
            busy: RefCell::new(0),
            current: RefCell::new(None),
            queued: RefCell::new(VecDeque::new()),
            inspection: RefCell::new(None),
            diagnostics: RefCell::new(None),
            diagnostics_view: RefCell::new(None),
            updates: RefCell::new(Vec::new()),
            icon_hint,
        });

        // Dropping anywhere on the window works, not only on the empty page: a
        // user who already has applications installed still needs to add one.
        this.install_drop_target(&root);

        {
            let this = this.clone();
            choose.connect_clicked(move |_| this.pick_file());
        }
        {
            let this = this.clone();
            copy_command.connect_clicked(move |_| {
                let text = this.banner_command.text().to_string();
                this.window.clipboard().set_text(&text);
                this.status.success("Copied to the clipboard.");
            });
        }
        {
            let this = this.clone();
            recheck.connect_clicked(move |_| this.check_readiness());
        }

        for name in ["settings", "diagnostics", "quit", "check-updates"] {
            let action = gio::SimpleAction::new(name, None);
            let this = this.clone();
            action.connect_activate(move |_, _| match name {
                "settings" => this.open_settings(),
                "diagnostics" => this.open_diagnostics(),
                "quit" => this.window.close(),
                _ => {
                    this.status.note("Checking the compatibility registry…");
                    this.start_busy();
                    this.ui.task(Task::CheckUpdates);
                }
            });
            window.add_action(&action);
        }

        this
    }

    /// Start handling worker messages. Returns immediately.
    pub fn run(self: &Rc<Self>, receiver: Receiver<Message>) {
        let this = self.clone();
        // A timer rather than an async runtime: polling a channel is a few
        // nanoseconds, and it keeps the GUI free of machinery it has no other
        // use for.
        glib::timeout_add_local(TICK, move || {
            while let Ok(message) = receiver.try_recv() {
                this.handle(message);
            }
            glib::ControlFlow::Continue
        });
    }

    // ------------------------------------------------------------------- input

    fn pick_file(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose a Windows program")
            .build();
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Windows programs"));
        for pattern in ["*.exe", "*.EXE", "*.msi", "*.MSI", "*.bat", "*.BAT"] {
            filter.add_pattern(pattern);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));

        let this = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            // Dismissing the chooser is not a failure and has nothing to report;
            // a file that cannot be read is worth a word.
            if let Ok(file) = result {
                match file.path() {
                    Some(path) => this.accept_file(path),
                    None => this.status.failure("That file cannot be read from disk."),
                }
            }
        });
    }

    /// A file offered from outside the window — a command-line argument, or a
    /// file manager's "Open with".
    pub fn offer_file(&self, path: PathBuf) {
        self.accept_file(path);
    }

    /// Bring the window to the front.
    pub fn present(&self) {
        self.window.present();
    }

    /// A dropped or chosen file: check it, then confirm before changing anything.
    ///
    /// Files that arrive while something else is in progress are queued rather
    /// than dropped on the floor, because dropping six installers at once is a
    /// reasonable thing to try.
    fn accept_file(&self, path: PathBuf) {
        if self.current.borrow().is_some() {
            let mut queued = self.queued.borrow_mut();
            queued.push_back(path.clone());
            let waiting = queued.len();
            drop(queued);
            self.status.note(format!(
                "Queued {} — {waiting} more waiting.",
                name_of(&path)
            ));
            return;
        }

        if !is_windows_program(&path) {
            self.status.failure(format!(
                "{} is not a Windows program. WinDrop accepts .exe, .msi and .bat files.",
                name_of(&path)
            ));
            return;
        }

        *self.current.borrow_mut() = Some(path.clone());
        // Reading a large installer takes a moment, and it is what turns an
        // abstract confirmation into a concrete one.
        self.status.note(format!("Reading {}…", name_of(&path)));
        self.start_busy();
        self.ui.task(Task::Inspect(path));
    }

    /// Let the next queued file through, if there is one.
    fn finish_current(&self) {
        *self.current.borrow_mut() = None;
        let next = self.queued.borrow_mut().pop_front();
        if let Some(path) = next {
            self.accept_file(path);
        }
    }

    fn confirm_install(self: &Rc<Self>, path: PathBuf) {
        let summary = self
            .inspection
            .borrow()
            .as_ref()
            .map(describe_inspection)
            .unwrap_or_default();

        let heading = gtk::Label::new(Some(&name_of(&path)));
        heading.add_css_class("title-2");
        heading.set_wrap(true);
        heading.set_xalign(0.0);

        let body = gtk::Label::new(Some(&format!(
            "{summary}\n\nWinDrop will build a private environment for it, run the installer, \
             and add it to your menu. Nothing is installed system-wide."
        )));
        body.set_wrap(true);
        body.set_xalign(0.0);
        body.add_css_class("dim-label");

        let silent =
            gtk::CheckButton::with_label("Install silently, without the installer's own window");
        silent.set_active(self.ui.config.borrow().install_silently);
        let remember = gtk::CheckButton::with_label("Remember this choice");

        let cancel = gtk::Button::with_label("Cancel");
        let install = gtk::Button::with_label("Install");
        install.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&install);

        let content = dialog_content(&[heading.upcast_ref()], &[&silent, &remember], &buttons);
        let dialog = self.dialog("Install this program?", 520, 0, &content);

        {
            let this = self.clone();
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| {
                dialog.close();
                this.status.note("Cancelled.");
                this.finish_current();
            });
        }
        {
            let this = self.clone();
            let dialog = dialog.clone();
            install.connect_clicked(move |_| {
                let silent_now = silent.is_active();
                if remember.is_active() {
                    this.set_silent_installs(silent_now);
                }
                dialog.close();
                this.status.note(format!("Installing {}…", name_of(&path)));
                this.start_busy();
                this.ui.task(Task::Install(path.clone()));
            });
        }
        dialog.present();
    }

    fn set_silent_installs(&self, silent: bool) {
        {
            let mut config = self.ui.config.borrow_mut();
            if config.install_silently == silent {
                return;
            }
            config.install_silently = silent;
        }
        self.save_config("Settings saved.");
    }

    fn save_config(&self, success: &str) {
        let config = self.ui.config.borrow();
        match config.save(&self.ui.paths.config_file()) {
            Ok(()) => self.status.success(success),
            Err(error) => self
                .status
                .failure(format!("Could not save settings: {error}")),
        }
    }

    // ---------------------------------------------------------------- messages

    fn handle(self: &Rc<Self>, message: Message) {
        match message {
            Message::Stage(text) => self.status.note(&text),
            Message::Success(text) => {
                self.stop_busy();
                self.status.success(&text);
            }
            Message::Warning(text) => {
                self.stop_busy();
                self.status.warning(&text);
            }
            Message::Failed(text) => {
                self.stop_busy();
                self.status.failure(&text);
            }

            Message::Apps(apps) => self.refresh_list(&apps),

            Message::Inspection(result) => {
                self.stop_busy();
                match *result {
                    Ok(inspection) => {
                        let path = self
                            .current
                            .borrow()
                            .clone()
                            .or_else(|| inspection.path.clone())
                            .unwrap_or_default();
                        *self.inspection.borrow_mut() = Some(inspection);
                        self.confirm_install(path);
                    }
                    Err(text) => {
                        self.status.failure(&text);
                        self.finish_current();
                    }
                }
            }

            Message::InstallFinished(result) => {
                self.stop_busy();
                match *result {
                    Ok(installed) => {
                        let mut message = if installed.attempts == 0 {
                            format!("{} is ready.", installed.name)
                        } else {
                            format!(
                                "{} is ready — the first {} did not work, so WinDrop recorded the \
                                 one that did.",
                                installed.name,
                                text::count("environment", installed.attempts)
                            )
                        };
                        match &installed.desktop_file {
                            Some(_) => message.push_str(" Find it in your menu."),
                            // Worth saying: without a menu entry the only way in is
                            // this window, and the user would not know why.
                            None => message.push_str(
                                " It could not be added to your menu, so launch it from here.",
                            ),
                        }
                        self.status.success(&message);
                        self.ui.task(Task::Refresh);
                    }
                    Err(text) => self.status.failure(&text),
                }
                self.finish_current();
            }

            Message::RemoveFinished(result) => {
                self.stop_busy();
                match *result {
                    Ok(name) => {
                        self.status
                            .success(format!("Removed {name} and everything it owned."));
                        self.ui.task(Task::Refresh);
                    }
                    Err(text) => self.status.failure(&text),
                }
            }

            Message::LaunchFinished(result) => {
                self.stop_busy();
                match *result {
                    Ok(name) => self.status.success(format!("Started {name}.")),
                    Err(text) => self.status.failure(&text),
                }
            }

            Message::Doctor(diagnostics) => {
                self.stop_busy();
                self.show_readiness(&diagnostics);
                let report = self.diagnostics_report(&diagnostics);
                if let Some(view) = self.diagnostics_view.borrow().as_ref() {
                    view.buffer().set_text(&report);
                }
                self.filter_apps_missing_wine(&diagnostics);
            }

            Message::Updates(updates) => {
                self.stop_busy();
                let count = updates.len();
                *self.updates.borrow_mut() = updates;
                match count {
                    0 => self
                        .status
                        .success("Every application is on the newest profile available."),
                    n => self.status.note(format!(
                        "{} have an updated compatibility profile. \
                         See Diagnostics for the details.",
                        text::count("application", n)
                    )),
                }
            }

            Message::Log { app, text } => self.show_log(&app, &text),
        }
    }

    // --------------------------------------------------------------- app list

    fn refresh_list(self: &Rc<Self>, apps: &[InstalledApp]) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for app in apps {
            self.list.append(&self.build_row(app));
        }
        if apps.is_empty() {
            self.stack.set_visible_child_name("empty");
        } else {
            self.stack.set_visible_child_name("apps");
        }
    }

    /// One application, as a row.
    fn build_row(self: &Rc<Self>, app: &InstalledApp) -> gtk::ListBoxRow {
        let runnable = app.is_runnable();

        let icon = match app.icon.as_ref().filter(|path| path.is_file()) {
            Some(path) => {
                let image = gtk::Image::from_file(path);
                image.set_pixel_size(32);
                image
            }
            None => {
                let image = gtk::Image::from_icon_name("application-x-executable");
                image.set_pixel_size(32);
                image.add_css_class("dim-label");
                if let Some(hint) = self.icon_hint {
                    image.set_tooltip_text(Some(hint));
                }
                image
            }
        };

        let name = gtk::Label::builder().xalign(0.0).build();
        name.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&app.name)));

        // The line under the name answers "will this work, and with what?" in
        // the words a user would use, not WinDrop's.
        let meta_text = if runnable {
            let mut parts = vec![app.id.clone(), app.arch.to_string(), app.strategy()];
            if !app.dependencies.is_empty() {
                parts.push(app.dependencies.join(", "));
            }
            parts.join(" · ")
        } else {
            format!(
                "{} · the program it recorded is missing; reinstall it",
                app.id
            )
        };
        let meta = gtk::Label::builder()
            .xalign(0.0)
            .label(meta_text)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        meta.add_css_class("dim-label");
        meta.add_css_class("caption");

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.append(&name);
        text.append(&meta);

        let launch = gtk::Button::with_label("Launch");
        launch.add_css_class("suggested-action");
        launch.set_sensitive(runnable);
        launch.set_tooltip_text(Some("Start this application, detached from WinDrop"));

        // A per-row action group: the menu belongs to one application, and
        // namespacing it keeps rows from interfering with each other.
        let group_name = format!("app{}", app.id);
        let group = gio::SimpleActionGroup::new();
        let details = gio::SimpleAction::new("details", None);
        let logs = gio::SimpleAction::new("logs", None);
        let stop = gio::SimpleAction::new("stop", None);
        let remove = gio::SimpleAction::new("remove", None);
        group.add_action(&details);
        group.add_action(&logs);
        group.add_action(&stop);
        group.add_action(&remove);
        self.window.insert_action_group(&group_name, Some(&group));

        let menu = gio::Menu::new();
        menu.append(Some("Details"), Some(&format!("{group_name}.details")));
        menu.append(Some("Show log"), Some(&format!("{group_name}.logs")));
        menu.append(Some("Stop everything"), Some(&format!("{group_name}.stop")));
        menu.append(Some("Remove…"), Some(&format!("{group_name}.remove")));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build();
        menu_button.add_css_class("flat");
        menu_button.set_tooltip_text(Some("More"));

        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row_box.set_margin_top(8);
        row_box.set_margin_bottom(8);
        row_box.set_margin_start(12);
        row_box.set_margin_end(12);
        row_box.append(&icon);
        row_box.append(&text);
        row_box.append(&launch);
        row_box.append(&menu_button);
        let row = gtk::ListBoxRow::builder()
            .activatable(false)
            .child(&row_box)
            .build();

        {
            let this = self.clone();
            let id = app.id.clone();
            let name = app.name.clone();
            launch.connect_clicked(move |_| {
                this.status.note(format!("Starting {name}…"));
                this.ui.task(Task::Launch(id.clone()));
            });
        }
        {
            let this = self.clone();
            let app = app.clone();
            details.connect_activate(move |_, _| this.show_details(&app));
        }
        {
            let this = self.clone();
            let app = app.clone();
            logs.connect_activate(move |_, _| {
                this.status
                    .note(format!("Reading the log for {}…", app.name));
                this.ui.task(Task::ReadLog {
                    app: app.id.clone(),
                    lines: 400,
                });
            });
        }
        {
            let this = self.clone();
            let id = app.id.clone();
            stop.connect_activate(move |_, _| this.ui.task(Task::Stop(id.clone())));
        }
        {
            let this = self.clone();
            let app = app.clone();
            remove.connect_activate(move |_, _| this.confirm_remove(&app));
        }

        row
    }

    fn confirm_remove(self: &Rc<Self>, app: &InstalledApp) {
        let heading = gtk::Label::new(Some(&format!("Remove {}?", app.name)));
        heading.add_css_class("title-2");
        heading.set_wrap(true);
        heading.set_xalign(0.0);
        let body = gtk::Label::new(Some(&format!(
            "Its environment, its menu entry and everything it wrote — {} — will be deleted. \
             Nothing else on your system is touched.",
            human_bytes(app.size_on_disk(&self.ui.paths))
        )));
        body.set_wrap(true);
        body.set_xalign(0.0);
        body.add_css_class("dim-label");

        let cancel = gtk::Button::with_label("Keep it");
        let remove = gtk::Button::with_label("Remove");
        remove.add_css_class("destructive-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&remove);

        let content = dialog_content(&[heading.upcast_ref()], &[&body], &buttons);
        let dialog = self.dialog("Remove this application?", 460, 0, &content);

        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let this = self.clone();
            let dialog = dialog.clone();
            let id = app.id.clone();
            let name = app.name.clone();
            remove.connect_clicked(move |_| {
                dialog.close();
                this.status.note(format!("Removing {name}…"));
                this.start_busy();
                this.ui.task(Task::Remove(id.clone()));
            });
        }
        dialog.present();
    }

    /// Everything WinDrop knows about one installation.
    fn show_details(&self, app: &InstalledApp) {
        let translation_layers = match (app.dxvk(), app.vkd3d_proton()) {
            (false, false) => "none".to_string(),
            (dxvk, vkd3d) => [dxvk.then_some("DXVK"), vkd3d.then_some("VKD3D-Proton")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", "),
        };
        let rows: Vec<(&str, String)> = vec![
            ("Name", app.name.clone()),
            ("Id", app.id.clone()),
            ("Architecture", app.arch.to_string()),
            ("Environment", app.strategy()),
            (
                "Profile",
                format!("{} ({})", app.profile_id, app.profile_source.label()),
            ),
            ("Windows version", app.windows_version().label().to_string()),
            ("Translation layers", translation_layers),
            (
                "Dependencies",
                if app.dependencies.is_empty() {
                    "none".to_string()
                } else {
                    app.dependencies.join(", ")
                },
            ),
            ("Program", app.main_exe_windows.clone()),
            ("On disk", human_bytes(app.size_on_disk(&self.ui.paths))),
            ("Installed", app.installed_at.clone()),
            (
                "Environment folder",
                app.prefix(&self.ui.paths).root().display().to_string(),
            ),
            (
                "Menu entry",
                app.desktop_file
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "none".to_string()),
            ),
        ];

        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(18)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        for (index, (label, value)) in rows.iter().enumerate() {
            let key = gtk::Label::builder().xalign(0.0).label(*label).build();
            key.add_css_class("dim-label");
            let val = gtk::Label::builder()
                .xalign(0.0)
                .label(value)
                .selectable(true)
                .wrap(true)
                .max_width_chars(60)
                .build();
            grid.attach(&key, 0, index as i32, 1, 1);
            grid.attach(&val, 1, index as i32, 1, 1);
        }

        let close = gtk::Button::with_label("Close");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_bottom(12);
        buttons.set_margin_end(18);
        buttons.append(&close);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&grid)
            .vexpand(true)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&scroller);
        if !app.notes.is_empty() {
            let notes = gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .label(&app.notes)
                .build();
            notes.add_css_class("dim-label");
            notes.set_margin_start(18);
            notes.set_margin_end(18);
            notes.set_margin_bottom(12);
            content.append(&notes);
        }
        content.append(&buttons);

        let dialog = self.dialog(&format!("{} — details", app.name), 640, 520, &content);
        {
            let dialog = dialog.clone();
            close.connect_clicked(move |_| dialog.close());
        }
        dialog.present();
    }

    fn show_log(&self, app: &str, text: &str) {
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .build();
        view.add_css_class("monospace");
        let buffer = view.buffer();
        buffer.set_text(text);
        // Show the end: the newest lines are the interesting ones.
        let mut end = buffer.end_iter();
        view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);

        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .label(format!(
                "The most recent run of {app}. Wine's own messages appear here too, \
                 and are usually harmless."
            ))
            .build();
        title.add_css_class("dim-label");
        title.set_margin_top(12);
        title.set_margin_start(12);
        title.set_margin_end(12);

        let close = gtk::Button::with_label("Close");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(12);
        buttons.set_margin_bottom(12);
        buttons.set_margin_end(12);
        buttons.append(&close);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&title);
        content.append(&scroll);
        content.append(&buttons);

        let dialog = self.dialog(&format!("{app} — log"), 820, 560, &content);
        {
            let dialog = dialog.clone();
            close.connect_clicked(move |_| dialog.close());
        }
        dialog.present();
    }

    // ---------------------------------------------------------------- readiness

    fn check_readiness(&self) {
        self.status.note("Checking what WinDrop needs…");
        self.start_busy();
        self.ui.task(Task::Doctor);
    }

    /// Update the banner, the status line, and whether anything can be installed.
    fn show_readiness(&self, diagnostics: &Diagnostics) {
        // The banner is for something that actually blocks work. A missing icon
        // tool is worth a line in Diagnostics and nowhere else.
        let blocking = diagnostics.wine.is_none()
            || diagnostics
                .missing(Necessity::Required)
                .iter()
                .any(|tool| tool.name != "wine");

        *self.diagnostics.borrow_mut() = Some(diagnostics.clone());
        if !blocking {
            self.banner.set_reveal_child(false);
            if diagnostics.wine.is_some() {
                self.status.success("Ready.");
            }
            return;
        }

        self.banner_text.set_text(
            diagnostics
                .priority_issue()
                .unwrap_or_else(|| "WinDrop is missing something it needs".to_string())
                .as_str(),
        );
        let missing = diagnostics.missing(Necessity::Recommended);
        self.banner_detail.set_text(&if missing.is_empty() {
            "Everything else is present.".to_string()
        } else {
            format!(
                "Missing: {}.",
                missing
                    .iter()
                    .map(|tool| format!("{} ({})", tool.name, tool.purpose))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
        banner_command_visibility(&self.banner_command, &self.banner_commands, diagnostics);
        self.banner.set_reveal_child(true);
        self.status
            .warning(format!("Not ready yet: {}", diagnostics.summary_line()));
    }

    fn filter_apps_missing_wine(&self, diagnostics: &Diagnostics) {
        // Every existing row was built when Wine may or may not have been
        // present; rebuilding is cheap and keeps Launch buttons honest.
        let _ = diagnostics;
        self.ui.task(Task::Refresh);
    }

    // ----------------------------------------------------------------- settings

    fn open_settings(self: &Rc<Self>) {
        let config = self.ui.config.borrow().clone();
        let previous_variant = config.wine_variant.clone();

        // A named build is not one of the three presets, so it is appended
        // rather than silently showing `stable`, which would be a lie.
        let mut wine_choices = vec![
            "stable".to_string(),
            "staging".to_string(),
            "system".to_string(),
        ];
        if let WineVariant::Build(name) = &config.wine_variant {
            wine_choices.push(name.clone());
        }
        let wine_labels: Vec<&str> = wine_choices.iter().map(|s| s.as_str()).collect();
        let wine = gtk::DropDown::builder()
            .model(&gtk::StringList::new(&wine_labels))
            .build();
        wine.set_selected(match config.wine_variant {
            WineVariant::Stable => 0,
            WineVariant::Staging => 1,
            WineVariant::System => 2,
            WineVariant::Build(_) => 3,
        });

        let sandbox =
            gtk::DropDown::from_strings(&["strict (bubblewrap)", "off (fastest, least isolated)"]);
        sandbox.set_selected(if config.sandbox == SandboxMode::Strict {
            0
        } else {
            1
        });

        let performance =
            gtk::DropDown::from_strings(&["balanced", "performance", "compatibility"]);
        performance.set_selected(match config.performance_mode {
            PerformanceMode::Balanced => 0,
            PerformanceMode::Performance => 1,
            PerformanceMode::Compatibility => 2,
        });

        let dxvk = switch(config.dxvk);
        let vkd3d = switch(config.vkd3d_proton);
        let registry = switch(config.allow_remote_registry);
        let updates = switch(config.auto_profile_updates);
        let silent = switch(config.install_silently);

        let grid = gtk::Grid::builder()
            .row_spacing(12)
            .column_spacing(18)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();

        let rows: [(&str, &str, &gtk::Widget); 8] = [
            (
                "Wine build",
                "Which Wine WinDrop prefers. A profile can still ask for another, and the fallback chain will try it.",
                wine.upcast_ref(),
            ),
            (
                "DXVK",
                "Translates Direct3D 9, 10 and 11 to Vulkan. Needed by games and anything that renders.",
                dxvk.upcast_ref(),
            ),
            (
                "VKD3D-Proton",
                "Translates Direct3D 12 to Vulkan. Needed by modern titles.",
                vkd3d.upcast_ref(),
            ),
            (
                "Isolation",
                "bubblewrap hides the rest of your home directory from Windows programs.",
                sandbox.upcast_ref(),
            ),
            (
                "Performance mode",
                "One switch for the usual trade-offs between latency, throughput and correctness.",
                performance.upcast_ref(),
            ),
            (
                "Community profiles",
                "Let WinDrop look up recipes other people have contributed.",
                registry.upcast_ref(),
            ),
            (
                "Automatic updates",
                "Look for newer recipes in the background. Nothing is reinstalled without asking.",
                updates.upcast_ref(),
            ),
            (
                "Silent installs",
                "Drive installers with their own silent flags instead of showing their window.",
                silent.upcast_ref(),
            ),
        ];
        for (row, (title, hint, widget)) in rows.iter().enumerate() {
            let key = gtk::Label::builder().xalign(0.0).label(*title).build();
            let hint = gtk::Label::builder()
                .xalign(0.0)
                .label(*hint)
                .wrap(true)
                .max_width_chars(46)
                .build();
            hint.add_css_class("dim-label");
            hint.add_css_class("caption");
            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.append(&key);
            text.append(&hint);
            grid.attach(&text, 0, row as i32, 1, 1);
            grid.attach(*widget, 1, row as i32, 1, 1);
        }

        let data_path = gtk::Label::builder()
            .xalign(0.0)
            .label(self.ui.paths.data_dir().display().to_string())
            .selectable(true)
            .wrap(true)
            .max_width_chars(46)
            .build();
        data_path.add_css_class("monospace");
        let data_note = gtk::Label::builder()
            .xalign(0.0)
            .label(
                "Everything WinDrop creates lives here. Start it with --data-dir to use another.",
            )
            .wrap(true)
            .max_width_chars(46)
            .build();
        data_note.add_css_class("dim-label");
        data_note.add_css_class("caption");
        let data_key = gtk::Label::builder()
            .xalign(0.0)
            .label("Data directory")
            .build();
        let data_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        data_box.append(&data_path);
        data_box.append(&data_note);
        grid.attach(&data_key, 0, rows.len() as i32, 1, 1);
        grid.attach(&data_box, 1, rows.len() as i32, 1, 1);

        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_bottom(12);
        buttons.set_margin_end(18);
        buttons.append(&cancel);
        buttons.append(&save);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(
            &gtk::ScrolledWindow::builder()
                .child(&grid)
                .vexpand(true)
                .build(),
        );
        content.append(&buttons);
        let dialog = self.dialog("WinDrop settings", 660, 560, &content);

        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let this = self.clone();
            let dialog = dialog.clone();
            save.connect_clicked(move |_| {
                {
                    let mut config = this.ui.config.borrow_mut();
                    config.wine_variant = match wine.selected() {
                        1 => WineVariant::Staging,
                        2 => WineVariant::System,
                        3 => match &previous_variant {
                            WineVariant::Build(name) => WineVariant::Build(name.clone()),
                            _ => WineVariant::Stable,
                        },
                        _ => WineVariant::Stable,
                    };
                    config.dxvk = dxvk.is_active();
                    config.vkd3d_proton = vkd3d.is_active();
                    config.sandbox = if sandbox.selected() == 0 {
                        SandboxMode::Strict
                    } else {
                        SandboxMode::Off
                    };
                    config.performance_mode = match performance.selected() {
                        1 => PerformanceMode::Performance,
                        2 => PerformanceMode::Compatibility,
                        _ => PerformanceMode::Balanced,
                    };
                    config.allow_remote_registry = registry.is_active();
                    config.auto_profile_updates = updates.is_active();
                    config.install_silently = silent.is_active();
                }
                this.save_config("Settings saved.");
                dialog.close();
            });
        }
        dialog.present();
    }

    // --------------------------------------------------------------- diagnostics

    fn open_diagnostics(self: &Rc<Self>) {
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .build();
        view.add_css_class("monospace");
        let report = match self.diagnostics.borrow().as_ref() {
            Some(diagnostics) => self.diagnostics_report(diagnostics),
            None => "Checking this machine…".to_string(),
        };
        view.buffer().set_text(&report);

        let refresh = gtk::Button::with_label("Check again");
        let copy = gtk::Button::with_label("Copy");
        let close = gtk::Button::with_label("Close");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(12);
        buttons.set_margin_bottom(12);
        buttons.set_margin_end(12);
        buttons.append(&refresh);
        buttons.append(&copy);
        buttons.append(&close);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(
            &gtk::ScrolledWindow::builder()
                .child(&view)
                .vexpand(true)
                .build(),
        );
        content.append(&buttons);
        let dialog = self.dialog("WinDrop diagnostics", 760, 600, &content);

        *self.diagnostics_view.borrow_mut() = Some(view.clone());
        {
            let this = self.clone();
            refresh.connect_clicked(move |_| this.check_readiness());
        }
        {
            let clipboard = dialog.clipboard();
            copy.connect_clicked(move |_| {
                let text = view
                    .buffer()
                    .text(
                        &view.buffer().start_iter(),
                        &view.buffer().end_iter(),
                        false,
                    )
                    .to_string();
                clipboard.set_text(&text);
            });
        }
        {
            let this = self.clone();
            let dialog = dialog.clone();
            close.connect_clicked(move |_| {
                *this.diagnostics_view.borrow_mut() = None;
                dialog.close();
            });
        }
        dialog.present();
        self.check_readiness();
    }

    fn diagnostics_report(&self, diagnostics: &Diagnostics) -> String {
        let mut text = diagnostics.report();
        let updates = self.updates.borrow();
        if !updates.is_empty() {
            text.push_str("\nUpdated profiles available\n");
            text.push_str("--------------------------\n");
            for update in updates.iter() {
                text.push_str(&format!("{}\n  {}\n", update.app_name, update.detail()));
            }
        }
        text.push_str(&format!(
            "\nVersion\n-------\nWinDrop {}\n",
            windrop_core::version()
        ));
        text
    }

    // ------------------------------------------------------------------ busy

    /// Show that something is happening.
    ///
    /// A counter rather than a flag: an install is followed immediately by a
    /// refresh, and the spinner must not stop in between.
    fn start_busy(&self) {
        *self.busy.borrow_mut() += 1;
        self.spinner.set_visible(true);
        self.spinner.start();
    }

    fn stop_busy(&self) {
        let mut busy = self.busy.borrow_mut();
        *busy = busy.saturating_sub(1);
        if *busy == 0 {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }
    }

    // ----------------------------------------------------------------- dialogs

    /// A modal window, sized the same way for every dialog in the window.
    fn dialog(&self, title: &str, width: i32, height: i32, content: &gtk::Box) -> gtk::Window {
        let mut builder = gtk::Window::builder()
            .title(title)
            .transient_for(&self.window)
            .modal(true)
            .default_width(width)
            .child(content);
        if height > 0 {
            builder = builder.default_height(height);
        }
        let dialog = builder.build();
        // Escape closes, which is what everyone expects of a dialog.
        let escape = gtk::EventControllerKey::new();
        {
            let dialog = dialog.clone();
            escape.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    dialog.close();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        dialog.add_controller(escape);
        dialog
    }

    /// Accept dropped files anywhere in the window.
    fn install_drop_target(self: &Rc<Self>, widget: &impl IsA<gtk::Widget>) {
        let weak = Rc::downgrade(self);
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        target.connect_drop(move |_target, value, _x, _y| {
            let Some(window) = weak.upgrade() else {
                return false;
            };
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            // Claim the drop only if at least one path could actually be read:
            // a drop WinDrop cannot use should be left for someone else.
            let mut handled = false;
            for file in list.files() {
                if let Some(path) = file.path() {
                    window.accept_file(path);
                    handled = true;
                }
            }
            handled
        });
        widget.add_controller(target);
    }
}

/// Collapse a message onto one line.
///
/// The status area is a single line, and errors from Wine and `winetricks` are
/// several: left alone they would either resize the window or be clipped.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a path looks like something WinDrop can install.
fn is_windows_program(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("exe") | Some("msi") | Some("bat")
    )
}

fn switch(active: bool) -> gtk::Switch {
    let switch = gtk::Switch::new();
    switch.set_active(active);
    switch.set_halign(gtk::Align::Start);
    switch
}

/// Assemble a dialog body: headings, then settings, then buttons at the bottom.
fn dialog_content(
    headings: &[&gtk::Widget],
    settings: &[&impl IsA<gtk::Widget>],
    buttons: &gtk::Box,
) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    for heading in headings {
        content.append(*heading);
    }
    for setting in settings {
        content.append(*setting);
    }
    content.append(buttons);
    content
}

/// Show or hide the "here is the command" part of the banner.
fn banner_command_visibility(command: &gtk::Label, commands: &gtk::Box, diagnostics: &Diagnostics) {
    match diagnostics.setup_command() {
        Some(text) => {
            command.set_text(&text);
            commands.set_visible(true);
        }
        None => {
            command.set_text("");
            commands.set_visible(false);
        }
    }
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// A one-line description of a Windows file, for the confirmation dialog.
fn describe_inspection(inspection: &PeInspection) -> String {
    let mut parts = vec![
        inspection.arch.to_string(),
        human_bytes(inspection.size_bytes),
    ];
    parts.push(if inspection.is_dll {
        "a library, not a program".to_string()
    } else if inspection.dotnet {
        "a .NET program".to_string()
    } else if inspection.gui {
        "a graphical program".to_string()
    } else {
        "a console program".to_string()
    });
    if inspection.signed {
        parts.push("digitally signed".to_string());
    }
    parts.join(" · ")
}

/// The status line under the list.
struct Status {
    label: gtk::Label,
}

impl Status {
    fn new() -> Self {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        Status { label }
    }

    fn note(&self, text: impl AsRef<str>) {
        self.set(text.as_ref(), &[]);
    }

    fn success(&self, text: impl AsRef<str>) {
        self.set(text.as_ref(), &["success"]);
    }

    fn warning(&self, text: impl AsRef<str>) {
        self.set(text.as_ref(), &["warning"]);
    }

    /// A failure, which may carry a blank-line-separated hint underneath.
    fn failure(&self, text: impl AsRef<str>) {
        self.set(&one_line(text.as_ref()), &["error"]);
    }

    fn set(&self, text: &str, classes: &[&str]) {
        for class in ["success", "warning", "error"] {
            self.label.remove_css_class(class);
        }
        for class in classes {
            self.label.add_css_class(class);
        }
        self.label.set_text(text);
        self.label.set_tooltip_text(Some(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_windows_program_extensions_are_accepted() {
        for good in ["a.exe", "SETUP.EXE", "installer.msi", "run.bat", "odd.Exe"] {
            assert!(
                is_windows_program(Path::new(good)),
                "{good} should be accepted"
            );
        }
        for bad in ["a.txt", "setup", "archive.tar.gz", "note.exe.txt", ""] {
            assert!(
                !is_windows_program(Path::new(bad)),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn a_message_is_flattened_onto_one_line() {
        let text = one_line("something went wrong\n\nInstall it with: sudo pacman -S wine");
        assert!(!text.contains('\n'), "{text:?}");
        assert!(text.contains("pacman"), "the hint must survive: {text:?}");
        assert!(
            !text.contains("  "),
            "runs of spaces should be collapsed: {text:?}"
        );
    }

    #[test]
    fn flattening_a_single_line_changes_nothing() {
        assert_eq!(one_line("Ready."), "Ready.");
        assert_eq!(one_line("  \t \n "), "");
    }

    #[test]
    fn a_filename_is_shown_without_its_directory() {
        assert_eq!(
            name_of(Path::new("/home/u/Downloads/Setup.exe")),
            "Setup.exe"
        );
        assert_eq!(name_of(Path::new("relative.exe")), "relative.exe");
    }
}

//! The window: keeps the UI in sync with what's chosen and runs jobs.

use crate::drives::{self, Drive, DriveKind};
use crate::engine::{self, ImageFormat, ImageInfo};
use crate::fmt;
use crate::job::{self, Event, Job, Outcome, Work};
use crate::plan::{self, Options, Plan, Source, Target};
use serde::Deserialize;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

/// How many bars the throughput chart shows.
const BARS: usize = 48;

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

fn app() -> Rc<App> {
    APP.with(|a| a.borrow().clone()).expect("the app is running")
}

/// Runs `f` on the UI thread; callable from any thread.
fn post(f: impl FnOnce(&App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || f(&app()));
}

pub fn run() -> Result<(), slint::PlatformError> {
    // Icons and a desktop entry in ~/.local on Linux, so menus and taskbars show DD-GUI's icon.
    crate::desktop::integrate();
    let ui = AppWindow::new()?;
    // Window class / Wayland app id, so desktops match the window to dd-gui.desktop.
    // Needs the platform that creating the window just set up; it only has to happen before showing it.
    let _ = slint::set_xdg_app_id("dd-gui");
    let instance = Rc::new(App {
        ui: ui.as_weak(),
        state: RefCell::new(State::default()),
        ticker: Timer::default(),
        poller: Timer::default(),
    });
    APP.with(|a| *a.borrow_mut() = Some(instance.clone()));

    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_zeros_available(plan::ZEROS_AVAILABLE);
    ui.on_choose_source_file(|| app().choose_source_file());
    ui.on_choose_target_file(|| app().choose_target_file());
    ui.on_open_picker(|for_source| app().open_picker(for_source));
    ui.on_pick_drive(|key| app().pick_drive(&key));
    ui.on_refresh_drives(|| {
        let app = app();
        app.show_drives();
        app.load_drives();
    });
    ui.on_settings_changed(|| app().refresh());
    ui.on_start(|| app().start());
    ui.on_confirm(|| app().launch());
    ui.on_mode_start(|| app().mode_start());
    ui.on_mode_cancel(|| app().mode_cancel());
    ui.on_cancel_job(|| app().cancel());
    ui.on_finish(|| app().finish());
    ui.on_reveal(|| app().reveal());
    ui.on_open_repository(|| open_url(env!("CARGO_PKG_REPOSITORY")));

    // `dd-gui some.iso` (e.g. "Open with" in a file manager) starts with that image chosen.
    if let Some(path) = std::env::args_os().nth(1).map(PathBuf::from).filter(|p| p.is_file()) {
        instance.set_source_file(path.canonicalize().unwrap_or(path));
    }

    instance.refresh();
    instance.load_drives();
    ui.run()
}

/// The drive layout a worker reports before a copy from a drive (`smart::Layout` as JSON).
#[derive(Deserialize)]
struct LayoutInfo {
    size: u64,
    partitions: Vec<PartInfo>,
    extents: Vec<ExtentInfo>,
}

#[derive(Deserialize)]
struct PartInfo {
    index: u32,
    fs: Option<String>,
    label: Option<String>,
    understood: bool,
}

#[derive(Deserialize)]
struct ExtentInfo {
    len: u64,
}

impl LayoutInfo {
    fn used(&self) -> u64 {
        self.extents.iter().map(|e| e.len).sum()
    }
}

/// A copy from a drive whose worker is running, waiting for "smart" or "sector by sector".
struct Pending {
    source: Drive,
    target: Target,
    /// The file name the user picked in the save dialog.
    chosen_file: Option<PathBuf>,
    /// Where each mode saves, when the target is a file.
    smart_file: Option<PathBuf>,
    full_file: Option<PathBuf>,
    /// What dd will copy sector by sector.
    full_total: Option<u64>,
    elevated: bool,
    layout: Option<LayoutInfo>,
}

#[derive(Default)]
struct State {
    // Choices are kept per tab, so flipping between tabs doesn't lose them.
    source_file: Option<(PathBuf, ImageInfo)>,
    source_drive: Option<Drive>,
    target_file: Option<PathBuf>,
    target_drive: Option<Drive>,
    drives: Vec<Drive>,
    loading: bool,
    picker_for_source: bool,
    /// A passing message for the banner, e.g. "The password prompt was dismissed."
    notice: Option<String>,
    /// Events from other jobs than this one are stale and get ignored.
    job_id: u64,
    job: Option<Job>,
    pending: Option<Pending>,
    run: Option<Run>,
    /// Start was clicked and the chosen drives are being checked again.
    rechecking: bool,
}

struct App {
    ui: slint::Weak<AppWindow>,
    state: RefCell<State>,
    /// Refreshes the clock on the job page.
    ticker: Timer,
    /// Re-scans drives while the picker is open, so plugged-in drives show up.
    poller: Timer,
}

impl App {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("the window is alive")
    }

    fn source(&self, ui: &AppWindow) -> Source {
        let st = self.state.borrow();
        match ui.get_source_mode() {
            0 => st.source_file.clone().map_or(Source::None, |(path, info)| Source::File { path, info }),
            1 => st.source_drive.clone().map_or(Source::None, Source::Drive),
            _ => Source::Zeros,
        }
    }

    fn target(&self, ui: &AppWindow) -> Target {
        let st = self.state.borrow();
        match ui.get_target_mode() {
            0 => st.target_drive.clone().map_or(Target::None, Target::Drive),
            _ => st.target_file.clone().map_or(Target::None, Target::File),
        }
    }

    fn options(&self, ui: &AppWindow) -> Result<Options, String> {
        let number = |text: SharedString, name: &str| -> Result<Option<u64>, String> {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            text.parse().map(Some).map_err(|_| format!("{name} has to be a whole number of blocks."))
        };
        Ok(Options {
            block_size: usize::try_from(ui.get_block_size() - 1).ok().and_then(|i| plan::BLOCK_SIZES.get(i)).copied(),
            noerror: ui.get_opt_noerror(),
            count: number(ui.get_count_text(), "Count")?,
            skip: number(ui.get_skip_text(), "Skip")?,
            seek: number(ui.get_seek_text(), "Seek")?,
        })
    }

    fn set_source_file(&self, path: PathBuf) {
        let info = engine::probe(&path).unwrap_or_else(|_| {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            ImageInfo { format: ImageFormat::Raw, size, uncompressed: Some(size), smart: None }
        });
        self.state.borrow_mut().source_file = Some((path, info));
        self.refresh();
    }

    /// Brings everything on the setup page up to date.
    fn refresh(&self) {
        let ui = self.ui();
        let source = self.source(&ui);
        let target = self.target(&ui);
        let options = self.options(&ui);
        let opts = options.clone().unwrap_or_default();
        let drives = self.state.borrow().drives.clone();
        let plan = Plan { source: &source, target: &target, opts: &opts, drives: &drives };

        let (glyph, title, subtitle, tag) = match &source {
            Source::File { path, info } => {
                let (subtitle, tag) = file_summary(path, info);
                (0, file_name(path), subtitle, tag)
            }
            Source::Drive(d) => (drive_glyph(d), d.name.clone(), drive_summary(d), String::new()),
            Source::Zeros => (5, "Zeros".to_owned(), "An endless stream of zero bytes, to wipe a drive".to_owned(), String::new()),
            Source::None if ui.get_source_mode() == 1 => {
                (1, "Choose a drive".to_owned(), "Back it up or clone it".to_owned(), String::new())
            }
            Source::None => (0, "Choose an image".to_owned(), "ISO, IMG, DMG, VHD, VMDK, QCOW2 or compressed".to_owned(), String::new()),
        };
        ui.set_source_set(!matches!(source, Source::None));
        ui.set_source_glyph(glyph);
        ui.set_source_title(title.into());
        ui.set_source_subtitle(subtitle.into());
        ui.set_source_tag(tag.into());

        let (glyph, title, subtitle) = match &target {
            Target::Drive(d) => (drive_glyph(d), d.name.clone(), drive_summary(d)),
            Target::File(path) => (6, file_name(path), folder(path)),
            Target::None if ui.get_target_mode() == 0 => {
                (1, "Choose a drive".to_owned(), "USB drive, SD card or disk".to_owned())
            }
            Target::None => (6, "Choose where to save".to_owned(), "Saves everything into an image file".to_owned()),
        };
        ui.set_target_set(!matches!(target, Target::None));
        ui.set_target_glyph(glyph);
        ui.set_target_title(title.into());
        ui.set_target_subtitle(subtitle.into());

        let tokens: Vec<Token> = plan
            .words()
            .into_iter()
            .map(|(key, value)| Token { key: key.into(), value: display_value(key, &value).into() })
            .collect();
        ui.set_command(ModelRc::new(VecModel::from(tokens)));
        ui.set_command_text(plan.command_line().into());
        ui.set_auto_summary(plan.auto_summary().into());

        let problem = options.err().or_else(|| plan.problem());
        let both_chosen = !matches!(source, Source::None) && !matches!(target, Target::None);
        ui.set_can_start(problem.is_none());
        let notice = self.state.borrow().notice.clone();
        let (banner, warning) = match (problem.filter(|_| both_chosen), notice, plan.note().filter(|_| both_chosen)) {
            (Some(problem), ..) => (problem, true),
            (None, Some(notice), _) => (notice, false),
            (None, None, Some(note)) => (note, false),
            _ => (String::new(), false),
        };
        ui.set_banner(banner.into());
        ui.set_banner_warning(warning);
        ui.set_action_text(plan.action().into());
    }

    fn clear_notice(&self) {
        self.state.borrow_mut().notice = None;
    }

    fn choose_source_file(&self) {
        let ui = self.ui();
        self.clear_notice();
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Choose an image")
            .add_filter(
                "Disk images",
                &[
                    "iso", "img", "raw", "bin", "dd", "hddimg", "sdcard", "gz", "xz", "zst", "zip", "bz2", "lz4",
                    "lzma", "7z", "tar", "dmg", "vhd", "vhdx", "vmdk", "qcow2", "qcow",
                ],
            )
            .add_filter("All files", &["*"])
            .set_parent(&ui.window().window_handle());
        let _ = slint::spawn_local(async move {
            if let Some(file) = dialog.pick_file().await {
                app().set_source_file(file.path().to_path_buf());
            }
        });
    }

    fn choose_target_file(&self) {
        let ui = self.ui();
        self.clear_notice();
        let suggested = match self.source(&ui) {
            // Without the characters Windows (or macOS, or Linux) won't have in a file name.
            Source::Drive(d) => format!("{}.img", d.name.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "-")),
            Source::File { path, .. } => format!("Copy of {}", file_name(&path)),
            _ => "image.img".to_owned(),
        };
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Save image as")
            .set_file_name(suggested)
            .set_parent(&ui.window().window_handle());
        if let Some(home) = std::env::home_dir() {
            dialog = dialog.set_directory(home);
        }
        let _ = slint::spawn_local(async move {
            if let Some(file) = dialog.save_file().await {
                let app = app();
                app.state.borrow_mut().target_file = Some(file.path().to_path_buf());
                app.refresh();
            }
        });
    }

    fn open_picker(&self, for_source: bool) {
        let ui = self.ui();
        self.clear_notice();
        self.state.borrow_mut().picker_for_source = for_source;
        ui.set_picker_for_source(for_source);
        self.show_drives();
        ui.set_picker_open(true);
        self.load_drives();
        // Listing drives means starting PowerShell on Windows, which takes a second or two.
        let every = Duration::from_secs(if cfg!(windows) { 4 } else { 2 });
        self.poller.start(TimerMode::Repeated, every, || {
            let app = app();
            if app.ui().get_picker_open() {
                app.load_drives();
            } else {
                app.poller.stop();
            }
        });
    }

    fn pick_drive(&self, key: &str) {
        let ui = self.ui();
        {
            let mut st = self.state.borrow_mut();
            let Some(drive) = st.drives.iter().find(|d| d.path == key).cloned() else { return };
            if st.picker_for_source {
                st.source_drive = Some(drive);
            } else {
                st.target_drive = Some(drive);
            }
        }
        ui.set_picker_open(false);
        self.poller.stop();
        self.refresh();
    }

    fn load_drives(&self) {
        {
            let mut st = self.state.borrow_mut();
            if st.loading {
                return;
            }
            st.loading = true;
        }
        std::thread::spawn(|| {
            let result = drives::list();
            post(move |app| app.drives_loaded(result));
        });
    }

    fn drives_loaded(&self, result: Result<Vec<Drive>, String>) {
        let ui = self.ui();
        let mut removed = None;
        {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            st.loading = false;
            match result {
                Ok(list) => {
                    ui.set_drives_error(SharedString::new());
                    // Keep the chosen drives current, and let go of unplugged ones.
                    let busy = st.job.is_some();
                    for chosen in [&mut st.source_drive, &mut st.target_drive] {
                        if let Some(drive) = chosen {
                            match list.iter().find(|d| d.path == drive.path) {
                                Some(fresh) => *drive = fresh.clone(),
                                None if !busy => removed = chosen.take().map(|d| d.name),
                                None => {}
                            }
                        }
                    }
                    st.drives = list;
                }
                Err(err) => ui.set_drives_error(err.into()),
            }
            if let Some(name) = &removed {
                st.notice = Some(format!("{name} was disconnected."));
            }
        }
        self.show_drives();
        self.refresh();
    }

    /// Fills the picker from the last scan.
    fn show_drives(&self) {
        let ui = self.ui();
        let st = self.state.borrow();
        let show_all = ui.get_show_all_drives();
        let for_source = st.picker_for_source;
        let needed = match self.source(&ui) {
            Source::File { info, .. } => info.smart.map(|s| s.disk_size).or(info.uncompressed),
            Source::Drive(d) => Some(d.size),
            _ => None,
        };
        let chosen = if for_source { &st.source_drive } else { &st.target_drive };
        // Only the drive actually in use on the other side counts, not one remembered on another tab.
        let other = match (for_source, self.source(&ui), self.target(&ui)) {
            (true, _, Target::Drive(d)) | (false, Source::Drive(d), _) => Some(d),
            _ => None,
        };
        let items: Vec<DriveItem> = st
            .drives
            .iter()
            .filter(|d| show_all || d.removable)
            .map(|d| {
                let is_other = other.as_ref().is_some_and(|o| o.path == d.path);
                let (note, disabled) = if is_other {
                    (if for_source { "TARGET" } else { "SOURCE" }, true)
                } else if d.system {
                    ("SYSTEM", !for_source)
                } else if d.read_only && !for_source {
                    ("READ-ONLY", true)
                } else if !for_source && needed.is_some_and(|s| s > d.size) {
                    ("TOO SMALL", true)
                } else if !d.mountpoints.is_empty() && !for_source {
                    ("MOUNTED", false)
                } else {
                    ("", false)
                };
                let mut detail = vec![d.bus.clone(), d.path.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
                if let Some(first) = d.mountpoints.first() {
                    detail.push(first.clone());
                }
                DriveItem {
                    key: d.path.clone().into(),
                    name: d.name.clone().into(),
                    detail: detail.join(" · ").into(),
                    size: fmt::bytes(d.size).into(),
                    kind: drive_glyph(d),
                    note: note.into(),
                    disabled,
                    selected: chosen.as_ref().is_some_and(|c| c.path == d.path),
                }
            })
            .collect();
        ui.set_drives(ModelRc::new(VecModel::from(items)));
        ui.set_drives_loading(st.loading && st.drives.is_empty());
    }

    fn busy(&self) -> bool {
        let st = self.state.borrow();
        st.job.is_some() || st.pending.is_some() || st.rechecking
    }

    /// Makes sure a chosen drive is still the same one: sticks get swapped, and
    /// /dev/sdb may be a different drive now than when it was picked.
    fn recheck(drive: &Drive, list: &[Drive]) -> Result<Drive, String> {
        match list.iter().find(|d| d.path == drive.path) {
            Some(d) if d.size == drive.size && d.name == drive.name => Ok(d.clone()),
            Some(_) => Err(format!("{} changed since you picked it. Choose the drive again.", drive.path)),
            None => Err(format!("{} is no longer connected.", drive.name)),
        }
    }

    fn start(&self) {
        let ui = self.ui();
        if self.busy() {
            return;
        }
        // An old message ("The password prompt was dismissed.") goes as soon as Start is clicked.
        self.clear_notice();
        self.refresh();
        let chosen = chosen_drives(&self.source(&ui), &self.target(&ui));
        if chosen.is_empty() {
            return self.start_checked();
        }
        // One fresh scan for both drives, off the UI thread: listing drives means starting
        // PowerShell on Windows, which takes a second or two.
        self.state.borrow_mut().rechecking = true;
        ui.set_starting(true);
        std::thread::spawn(move || {
            let result = drives::list();
            post(move |app| app.rechecked(chosen, result));
        });
    }

    /// Start, continued with a fresh list of drives.
    fn rechecked(&self, chosen: Vec<Drive>, result: Result<Vec<Drive>, String>) {
        let ui = self.ui();
        self.state.borrow_mut().rechecking = false;
        ui.set_starting(false);
        // Something else was picked in the meantime: that's for another click on Start.
        let now = chosen_drives(&self.source(&ui), &self.target(&ui));
        if now.iter().map(|d| &d.path).ne(chosen.iter().map(|d| &d.path)) {
            return;
        }
        let list = match result {
            Ok(list) => list,
            Err(err) => {
                self.state.borrow_mut().notice = Some(err);
                self.refresh();
                return;
            }
        };
        self.state.borrow_mut().drives = list.clone();
        for drive in &chosen {
            let fresh = Self::recheck(drive, &list);
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            // Go on with what the drive holds now: a partition mounted (or a volume
            // given a letter) since it was picked must be unmounted (locked) too.
            for slot in [&mut st.source_drive, &mut st.target_drive] {
                if slot.as_ref().is_some_and(|d| d.path == drive.path) {
                    *slot = fresh.clone().ok();
                }
            }
            if let Err(err) = fresh {
                st.notice = Some(err);
                drop(guard);
                self.refresh();
                return;
            }
        }
        self.start_checked();
    }

    /// Start, once any chosen drives are known to still be the ones picked.
    fn start_checked(&self) {
        let ui = self.ui();
        let (source, target) = (self.source(&ui), self.target(&ui));
        let Ok(opts) = self.options(&ui) else { return };
        let drives = self.state.borrow().drives.clone();
        if (Plan { source: &source, target: &target, opts: &opts, drives: &drives }).problem().is_some() {
            return;
        }
        if let Source::Drive(drive) = &source {
            return self.begin_drive_copy(drive.clone(), target, &opts);
        }
        let Target::Drive(drive) = &target else {
            self.launch();
            return;
        };
        ui.set_confirm_title(format!("Erase {}?", drive.name).into());
        let with = if matches!(source, Source::Zeros) { " with zeros" } else { "" };
        ui.set_confirm_body(
            format!(
                "Everything on this {} drive will be overwritten{with}. This can't be undone.",
                fmt::bytes(drive.size)
            )
            .into(),
        );
        ui.set_confirm_detail(erase_detail(drive).into());
        ui.set_confirm_action(if matches!(source, Source::Zeros) { "Wipe drive" } else { "Erase and write" }.into());
        ui.set_confirm_open(true);
    }

    /// Copies from a drive start with the worker reading the drive's layout, so
    /// the choice between a smart copy and sector by sector shows real numbers.
    fn begin_drive_copy(&self, drive: Drive, target: Target, opts: &Options) {
        let ui = self.ui();
        // A smart copy saves a zstd image next to the name the user picked.
        let (smart_file, full_file) = match &target {
            Target::File(path) => {
                let name = path.to_string_lossy();
                match name.strip_suffix(".zst") {
                    Some(raw) => (Some(path.clone()), Some(PathBuf::from(raw))),
                    None => (Some(PathBuf::from(format!("{name}.zst"))), Some(path.clone())),
                }
            }
            _ => (None, None),
        };
        // Sector by sector is plain dd, into the raw image name.
        let full_target = full_file.clone().map_or_else(|| target.clone(), Target::File);
        let source = Source::Drive(drive.clone());
        let drives = self.state.borrow().drives.clone();
        let plan = Plan { source: &source, target: &full_target, opts, drives: &drives };
        let elevated = plan.needs_elevation();
        let to = match (&target, &smart_file) {
            (Target::Drive(d), _) => d.io_path.clone(),
            (_, Some(file)) => file.to_string_lossy().into_owned(),
            _ => return,
        };
        let mut unmount = Vec::new();
        let mut lock = Vec::new();
        // Unmounting keeps the copy consistent, but the running system's drive can't be.
        // (Windows: locking its volumes does the same.)
        if !drive.system {
            unmount.extend(drive.unmount.iter().cloned());
            lock.extend(drive.lock.iter().cloned());
        }
        if let Target::Drive(d) = &target {
            unmount.extend(d.unmount.iter().cloned());
            lock.extend(d.lock.iter().cloned());
        }
        let spec = job::Spec {
            work: Work::Copy {
                mode: "smart",
                from: drive.io_path.clone(),
                to,
                ask: true,
                block_size: opts.block_size,
                size: None,
                dd_args: plan.args(),
            },
            elevate: elevated,
            unmount,
            lock,
            // dd flushes by itself only with conv=fsync; DD-GUI's copier gets told explicitly.
            sync: match &target {
                Target::Drive(d) => Some(d.io_path.clone()),
                _ => None,
            },
        };
        // Smart copies read whole file systems, so dd's count/skip/seek can't apply to them.
        let smart_available = opts.count.is_none() && opts.skip.is_none() && opts.seek.is_none();

        ui.set_mode_title(format!("Copy {}", drive.name).into());
        ui.set_mode_action(if matches!(target, Target::Drive(_)) { "Erase and copy" } else { "Start copy" }.into());
        ui.set_mode_subtitle(drive_summary(&drive).into());
        ui.set_mode_status(if elevated { "Waiting for permission…" } else { "Reading the drive's layout…" }.into());
        ui.set_mode_loading(true);
        ui.set_smart_available(smart_available);
        ui.set_mode_choice(if smart_available { 0 } else { 1 });
        ui.set_mode_open(true);
        ui.set_starting(true);

        let chosen_file = match &target {
            Target::File(path) => Some(path.clone()),
            _ => None,
        };
        let pending = Pending {
            source: drive,
            target,
            chosen_file,
            smart_file,
            full_file,
            full_total: plan.total_bytes(),
            elevated,
            layout: None,
        };
        let id = {
            let mut st = self.state.borrow_mut();
            st.job_id += 1;
            st.pending = Some(pending);
            st.run = None;
            st.job_id
        };
        match job::start(spec, move |event| post(move |app| app.on_event(id, event))) {
            Ok(job) => self.state.borrow_mut().job = Some(job),
            Err(err) => {
                self.state.borrow_mut().pending = None;
                ui.set_mode_open(false);
                ui.set_starting(false);
                self.state.borrow_mut().notice = Some(err);
                self.refresh();
            }
        }
    }

    /// Fills the copy-mode dialog once the worker has read the drive.
    fn show_modes(&self, layout: LayoutInfo) {
        let ui = self.ui();
        let mut st = self.state.borrow_mut();
        let Some(pending) = st.pending.as_mut() else { return };
        let (size, used) = (layout.size, layout.used());
        let to_file = matches!(pending.target, Target::File(_));

        let parts = layout.partitions.len();
        let table = match parts {
            0 => "no partitions".to_owned(),
            1 => "1 partition".to_owned(),
            n => format!("{n} partitions"),
        };
        ui.set_mode_subtitle(format!("{} · {table}", drive_summary(&pending.source)).into());

        ui.set_smart_figure(fmt::bytes(used).into());
        ui.set_full_figure(fmt::bytes(size).into());
        ui.set_smart_body(
            if to_file {
                "Only the space your files use. Saved compressed, and written back quickly by DD-GUI."
            } else {
                "Only the space your files use, so it's much faster. The target's free space is left alone."
            }
            .into(),
        );
        ui.set_full_body(
            if to_file {
                "Every byte, including free space and deleted files. Works with any file system. As big as the drive."
            } else {
                "Every byte, including free space and deleted files. Works with any file system or encryption."
            }
            .into(),
        );
        let mut read: Vec<String> = Vec::new();
        for fs in layout.partitions.iter().filter(|p| p.understood).filter_map(|p| p.fs.as_deref()) {
            let name = fmt::fs_name(fs);
            if !read.contains(&name) {
                read.push(name);
            }
        }
        ui.set_smart_foot(if read.is_empty() { "nothing it can read".to_owned() } else { format!("reads {}", read.join(" · ")) }.into());
        ui.set_full_foot("dd · every sector".into());

        let skipped: Vec<String> = layout
            .partitions
            .iter()
            .filter(|p| !p.understood)
            .map(|p| {
                let name = p.label.clone().unwrap_or_else(|| format!("partition {}", p.index));
                match &p.fs {
                    Some(fs) => format!("{name} ({})", fmt::fs_name(fs)),
                    None => name,
                }
            })
            .collect();
        let mut note = String::new();
        if pending.source.system {
            note.push_str("This drive runs your system, so files may change while it's copied. ");
        }
        if !skipped.is_empty() {
            note.push_str(&format!("Copied in full either way: {}.", skipped.join(", ")));
        }
        ui.set_mode_note(note.trim().into());
        // The save dialog only asked about the name the user typed; say so before
        // replacing the other one (backup.img vs backup.img.gz).
        let output = |file: &Option<PathBuf>| match file {
            Some(path) if path.exists() && Some(path) != pending.chosen_file.as_ref() => {
                format!("{}  (replaces the existing file)", file_name(path))
            }
            Some(path) => file_name(path),
            None => String::new(),
        };
        ui.set_smart_output(output(&pending.smart_file).into());
        ui.set_full_output(output(&pending.full_file).into());
        if !ui.get_smart_available() {
            note = format!("{note} Smart copy is off while count, skip or seek are set in Advanced.");
            ui.set_mode_note(note.trim().into());
        }
        match &pending.target {
            Target::Drive(d) => {
                ui.set_mode_erase(format!("Everything on {} ({}) will be erased.", d.name, d.path).into());
                ui.set_mode_action("Erase and copy".into());
            }
            _ => {
                ui.set_mode_erase(SharedString::new());
                ui.set_mode_action("Start copy".into());
            }
        }
        pending.layout = Some(layout);
        ui.set_mode_loading(false);
    }

    fn mode_start(&self) {
        let ui = self.ui();
        let smart = ui.get_mode_choice() == 0 && ui.get_smart_available();
        let (pending, id) = {
            let mut st = self.state.borrow_mut();
            if !st.pending.as_ref().is_some_and(|p| p.layout.is_some()) {
                return;
            }
            let Some(pending) = st.pending.take() else { return };
            (pending, st.job_id)
        };
        let Some(layout) = &pending.layout else { return };
        let file = if smart { pending.smart_file.clone() } else { pending.full_file.clone() };
        // Create the image as ourselves; an elevated worker would otherwise leave a root-owned file.
        if let Some(path) = &file
            && pending.elevated
            && let Err(err) = std::fs::File::create(path)
        {
            if let Some(job) = self.state.borrow_mut().job.take() {
                job.cancel();
            }
            ui.set_mode_open(false);
            ui.set_starting(false);
            self.state.borrow_mut().notice = Some(format!("Couldn't create {}: {err}", path.display()));
            self.refresh();
            return;
        }
        let target_name = match (&pending.target, &file) {
            (Target::Drive(d), _) => d.name.clone(),
            (_, Some(path)) => file_name(path),
            _ => String::new(),
        };
        let (from, to) = same_names(pending.source.name.clone(), target_name, &pending.source, &pending.target);
        let run = Run {
            total: if smart { Some(layout.used()) } else { pending.full_total },
            elevated: pending.elevated,
            started: true,
            syncing: false,
            copying_since: None,
            done: 0,
            written: None,
            samples: VecDeque::new(),
            mark: None,
            history: Vec::new(),
            verb: if smart { "Copying used space" } else if file.is_some() { "Reading" } else { "Cloning" },
            action: if file.is_some() { "Create image" } else { "Clone drive" },
            target_file: file,
            compressed_units: false,
            // A smart image's output is compressed: judge speed by what was read.
            output_is_compressed: smart && pending.smart_file.is_some(),
            skipped: smart.then(|| layout.size.saturating_sub(layout.used())),
            unmounted_source: (!pending.source.system && !(pending.source.unmount.is_empty() && pending.source.lock.is_empty()))
                .then(|| pending.source.clone()),
        };
        {
            let mut st = self.state.borrow_mut();
            if st.job_id != id {
                return;
            }
            st.run = Some(run);
            if let Some(job) = &st.job {
                job.answer(if smart { "smart" } else { "full" });
            }
        }
        ui.set_mode_open(false);
        ui.set_starting(false);
        ui.set_result(0);
        ui.set_run_cancelling(false);
        ui.set_run_route(format!("{from}  →  {to}").into());
        ui.set_run_progress(0.0);
        ui.set_page(1);
        self.render_run();
        self.ticker.start(TimerMode::Repeated, Duration::from_millis(500), || app().render_run());
    }

    fn mode_cancel(&self) {
        let ui = self.ui();
        {
            let mut st = self.state.borrow_mut();
            // A click on the dialog while it fades out after "Start copy" must not stop the copy.
            if st.pending.is_none() {
                return;
            }
            st.pending = None;
            if let Some(job) = st.job.take() {
                job.cancel();
            }
        }
        ui.set_mode_open(false);
        ui.set_starting(false);
        self.refresh();
    }

    fn launch(&self) {
        let ui = self.ui();
        if self.busy() {
            return;
        }
        let (source, target) = (self.source(&ui), self.target(&ui));
        let Ok(opts) = self.options(&ui) else { return };
        let drives = self.state.borrow().drives.clone();
        let plan = Plan { source: &source, target: &target, opts: &opts, drives: &drives };
        if plan.problem().is_some() {
            return;
        }
        let elevate = plan.needs_elevation();
        let (unmount, lock) = match &target {
            Target::Drive(d) => (d.unmount.clone(), d.lock.clone()),
            _ => Default::default(),
        };
        // Create the image as ourselves; an elevated dd would otherwise leave a root-owned file.
        if let Target::File(path) = &target
            && elevate
            && let Err(err) = std::fs::File::create(path)
        {
            self.state.borrow_mut().notice = Some(format!("Couldn't create {}: {err}", path.display()));
            self.refresh();
            return;
        }

        let restore = plan.restores_image();
        let work = match (&source, &target) {
            (Source::File { path, .. }, Target::Drive(d)) if restore => Work::Copy {
                mode: "restore",
                from: path.to_string_lossy().into_owned(),
                to: d.io_path.clone(),
                ask: false,
                block_size: opts.block_size,
                size: None,
                dd_args: Vec::new(),
            },
            // Windows has no /dev/zero: DD-GUI's copier writes the zeros there.
            (Source::Zeros, target) if cfg!(windows) => Work::Copy {
                mode: "zeros",
                from: String::new(),
                to: match target {
                    Target::Drive(d) => d.io_path.clone(),
                    Target::File(path) => path.to_string_lossy().into_owned(),
                    Target::None => return,
                },
                ask: false,
                block_size: opts.block_size,
                size: plan.total_bytes(),
                dd_args: Vec::new(),
            },
            _ => Work::Dd(plan.args()),
        };
        let sync = plan.sync_after();
        // The copier counts compressed bytes read for the stream formats it unpacks itself;
        // virtual disks, archives and the other formats count bytes written.
        let compressed_units = matches!(&source, Source::File { info, .. }
            if restore && info.smart.is_none()
                && matches!(info.format, ImageFormat::Gzip | ImageFormat::Xz | ImageFormat::Zstd | ImageFormat::Zip));
        let verb = match (&source, &target) {
            (Source::Zeros, _) => "Wiping",
            (_, Target::Drive(_)) => "Writing",
            _ => "Copying",
        };
        let route = format!("{}  →  {}", endpoint_name(&source), target_name(&target));
        let run = Run {
            total: plan.total_bytes().filter(|_| !restore),
            elevated: elevate,
            started: false,
            syncing: false,
            copying_since: None,
            done: 0,
            written: None,
            samples: VecDeque::new(),
            mark: None,
            history: Vec::new(),
            verb,
            action: plan.action(),
            target_file: match &target {
                Target::File(p) => Some(p.clone()),
                _ => None,
            },
            compressed_units,
            output_is_compressed: false,
            skipped: None,
            unmounted_source: None,
        };
        let spec = job::Spec { work, elevate, unmount, lock, sync };
        let id = {
            let mut st = self.state.borrow_mut();
            st.job_id += 1;
            st.run = Some(run);
            st.job_id
        };
        ui.set_result(0);
        ui.set_run_cancelling(false);
        ui.set_run_route(route.into());
        ui.set_run_progress(0.0);
        ui.set_page(1);
        self.render_run();

        match job::start(spec, move |event| post(move |app| app.on_event(id, event))) {
            Ok(job) => {
                self.state.borrow_mut().job = Some(job);
                self.ticker.start(TimerMode::Repeated, Duration::from_millis(500), || app().render_run());
            }
            Err(err) => self.finish_run(Outcome::Failed(err)),
        }
    }

    fn on_event(&self, id: u64, event: Event) {
        if self.state.borrow().job_id != id {
            return;
        }
        if let Event::Finished(outcome) = event {
            let waiting = self.state.borrow().pending.is_some();
            return if waiting { self.finish_pending(outcome) } else { self.finish_run(outcome) };
        }
        if let Event::Layout(json) = &event {
            match serde_json::from_str::<LayoutInfo>(json) {
                Ok(layout) => self.show_modes(layout),
                Err(err) => {
                    self.mode_cancel();
                    self.state.borrow_mut().notice = Some(format!("Couldn't read the drive's layout: {err}"));
                    self.refresh();
                }
            }
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            if st.pending.is_some() {
                match event {
                    Event::Started => self.ui().set_mode_status("Reading the drive's layout…".into()),
                    // Nothing may be copied before the user picked a mode. Stop right away.
                    Event::Copying | Event::Total(_) | Event::Progress { .. } | Event::Syncing => {
                        drop(st);
                        self.mode_cancel();
                        self.state.borrow_mut().notice =
                            Some("The copy started before a mode was chosen, so DD-GUI stopped it.".into());
                        self.refresh();
                    }
                    Event::Layout(_) | Event::Finished(_) => {}
                }
                return;
            }
            let Some(run) = st.run.as_mut() else { return };
            match event {
                Event::Started => run.started = true,
                Event::Copying => {
                    run.started = true;
                    run.copying_since = Some(Instant::now());
                }
                Event::Total(total) => run.total = Some(total),
                Event::Progress { done, written } => run.record(done, written),
                Event::Syncing => run.syncing = true,
                Event::Layout(_) | Event::Finished(_) => {}
            }
        }
        self.render_run();
    }

    /// The worker stopped before the copy mode was chosen.
    fn finish_pending(&self, outcome: Outcome) {
        let ui = self.ui();
        {
            let mut st = self.state.borrow_mut();
            st.pending = None;
            st.job = None;
            st.notice = match outcome {
                Outcome::NotAuthorized(message) => Some(message),
                Outcome::Failed(detail) => Some(format!("Couldn't read the drive: {}", detail.lines().last().unwrap_or(""))),
                Outcome::Success | Outcome::Cancelled => None,
            };
        }
        ui.set_mode_open(false);
        ui.set_starting(false);
        self.refresh();
    }

    fn render_run(&self) {
        let ui = self.ui();
        let st = self.state.borrow();
        let Some(run) = st.run.as_ref() else { return };
        let done_copying = run.syncing || run.total.is_some_and(|t| t > 0 && run.done >= t);
        let phase = if !run.started {
            if run.elevated { "Waiting for permission" } else { "Starting" }
        } else if run.copying_since.is_none() {
            "Getting the drive ready"
        } else if done_copying {
            "Flushing to disk"
        } else {
            run.verb
        };
        ui.set_run_phase(phase.into());
        ui.set_run_spinning(run.copying_since.is_none() || run.total.is_none() || done_copying);

        let fraction = run.total.filter(|&t| t > 0).map(|t| (run.done as f64 / t as f64).min(1.0));
        ui.set_run_progress(fraction.unwrap_or(0.0) as f32);
        ui.set_run_percent(
            match fraction {
                Some(f) if run.copying_since.is_some() => format!("{}%", (f * 100.0).floor()),
                _ => "–".to_owned(),
            }
            .into(),
        );
        ui.set_run_amount(
            match run.total {
                _ if run.compressed_units => format!("{} written", fmt::bytes(run.bytes_written())),
                Some(total) => format!("{} of {}", fmt::bytes(run.done), fmt::bytes(total)),
                None => format!("{} written", fmt::bytes(run.done)),
            }
            .into(),
        );

        let speed = run.speed();
        let eta = match (run.total, run.unit_rate()) {
            (Some(total), Some(rate)) if rate > 0.0 && !done_copying => {
                fmt::duration(total.saturating_sub(run.done) as f64 / rate)
            }
            _ => "–".to_owned(),
        };
        set_stats(
            &ui,
            &[
                ("SPEED", speed.map_or("–".to_owned(), fmt::speed)),
                ("TIME LEFT", eta),
                ("ELAPSED", fmt::duration(run.elapsed().as_secs_f64())),
            ],
        );

        // The most recent seconds, newest on the right.
        let recent = &run.history[run.history.len().saturating_sub(BARS)..];
        let mut bars = vec![0.0; BARS - recent.len()];
        bars.extend_from_slice(recent);
        ui.set_run_bars(normalized(bars));
    }

    fn cancel(&self) {
        if let Some(job) = &self.state.borrow().job {
            job.cancel();
            self.ui().set_run_cancelling(true);
        }
    }

    fn finish_run(&self, outcome: Outcome) {
        self.ticker.stop();
        let ui = self.ui();
        let run = {
            let mut st = self.state.borrow_mut();
            st.job = None;
            st.run.take()
        };
        let Some(run) = run else { return };
        match run.unmounted_source.clone() {
            // The worker unmounted the drive it copied from: mount it again for the user (off the
            // UI thread, since it waits for the OS), then list the drives with their mount points.
            Some(source) => {
                std::thread::spawn(move || {
                    let result = drives::remount(&source);
                    post(move |app| {
                        if let Err(err) = result {
                            app.state.borrow_mut().notice = Some(format!("Couldn't mount {} again: {err}", source.name));
                            app.refresh();
                        }
                        app.load_drives();
                    });
                });
            }
            None => self.load_drives(),
        }
        let elapsed = run.elapsed().as_secs_f64();
        let written = run.bytes_moved();
        match outcome {
            Outcome::NotAuthorized(message) => {
                ui.set_page(0);
                self.state.borrow_mut().notice = Some(message);
                self.refresh();
            }
            Outcome::Success => {
                ui.set_result(1);
                ui.set_result_title(success_title(run.action).into());
                let average = if elapsed > 0.0 { written as f64 / elapsed } else { 0.0 };
                if run.output_is_compressed {
                    set_stats(
                        &ui,
                        &[
                            ("COPIED", fmt::bytes(written)),
                            ("IMAGE SIZE", fmt::bytes(run.bytes_written())),
                            ("TOOK", fmt::duration(elapsed)),
                        ],
                    );
                } else {
                    set_stats(
                        &ui,
                        &[("COPIED", fmt::bytes(written)), ("AVERAGE", fmt::speed(average)), ("TOOK", fmt::duration(elapsed))],
                    );
                }
                if let Some(skipped) = run.skipped.filter(|&s| s > 0) {
                    let route = ui.get_run_route();
                    ui.set_run_route(format!("{route}\nSkipped {} of free space.", fmt::bytes(skipped)).into());
                }
                ui.set_run_bars(normalized(overview(&run.history)));
                ui.set_result_can_reveal(run.target_file.is_some());
                self.state.borrow_mut().run = Some(run);
            }
            Outcome::Failed(detail) => {
                ui.set_result(2);
                ui.set_result_title("Something went wrong".into());
                ui.set_result_body(
                    match written {
                        0 => "It stopped before copying anything.".to_owned(),
                        n => format!("It stopped after copying {}.", fmt::bytes(n)),
                    }
                    .into(),
                );
                ui.set_result_detail(detail.into());
                set_stats(&ui, &[]);
                ui.set_result_can_reveal(false);
            }
            Outcome::Cancelled => {
                ui.set_result(3);
                ui.set_result_title("Cancelled".into());
                ui.set_result_body(
                    match written {
                        0 => "Nothing was written.".to_owned(),
                        n => format!("Stopped after {}. The target only holds part of the data now.", fmt::bytes(n)),
                    }
                    .into(),
                );
                set_stats(&ui, &[]);
                ui.set_run_bars(normalized(overview(&run.history)));
                ui.set_result_can_reveal(false);
            }
        }
    }

    fn finish(&self) {
        let ui = self.ui();
        self.state.borrow_mut().run = None;
        ui.set_page(0);
        self.refresh();
    }

    fn reveal(&self) {
        let st = self.state.borrow();
        let Some(path) = st.run.as_ref().and_then(|r| r.target_file.clone()) else { return };
        reveal_in_file_manager(&path);
    }
}

/// The drives among a source and a target, in that order.
fn chosen_drives(source: &Source, target: &Target) -> Vec<Drive> {
    let mut drives = Vec::new();
    if let Source::Drive(d) = source {
        drives.push(d.clone());
    }
    if let Target::Drive(d) = target {
        drives.push(d.clone());
    }
    drives
}

fn set_stats(ui: &AppWindow, stats: &[(&str, String)]) {
    let tokens: Vec<Token> = stats.iter().map(|(k, v)| Token { key: (*k).into(), value: v.into() }).collect();
    ui.set_run_stats(ModelRc::new(VecModel::from(tokens)));
}

struct Run {
    /// What `done` counts toward.
    total: Option<u64>,
    elevated: bool,
    started: bool,
    syncing: bool,
    copying_since: Option<Instant>,
    done: u64,
    /// Bytes written, when progress counts something else (compressed input).
    written: Option<u64>,
    /// Recent (time, done, written) readings, for the current speed.
    samples: VecDeque<(Instant, u64, u64)>,
    /// Where the last chart bar ended.
    mark: Option<(Instant, u64)>,
    /// Write speed per second or so, for the chart.
    history: Vec<f64>,
    verb: &'static str,
    action: &'static str,
    target_file: Option<PathBuf>,
    /// Progress counts compressed bytes read rather than bytes written.
    compressed_units: bool,
    /// Bytes written are compressed output, so speed and totals go by bytes read.
    output_is_compressed: bool,
    /// Free space a smart copy didn't need to copy.
    skipped: Option<u64>,
    /// A source drive the worker unmounted, to mount again afterwards.
    unmounted_source: Option<Drive>,
}

impl Run {
    fn bytes_written(&self) -> u64 {
        self.written.unwrap_or(self.done)
    }

    /// The bytes that measure speed: what was read for compressed output, else what was written.
    fn bytes_moved(&self) -> u64 {
        if self.output_is_compressed { self.done } else { self.bytes_written() }
    }

    fn record(&mut self, done: u64, written: Option<u64>) {
        let now = Instant::now();
        self.done = done;
        self.written = written.or(self.written);
        let written = self.bytes_moved();
        // Chart one bar per second or so; a final report right after the last
        // periodic one would otherwise show up as a spike.
        let mark = self.mark.or(self.copying_since.map(|t| (t, 0)));
        if let Some((then, before)) = mark {
            let secs = now.duration_since(then).as_secs_f64();
            if secs >= 0.9 {
                self.history.push(written.saturating_sub(before) as f64 / secs);
                self.mark = Some((now, written));
            }
        }
        self.samples.push_back((now, done, written));
        while self.samples.len() > 8 {
            self.samples.pop_front();
        }
    }

    /// Recent change per second of (done, written).
    fn rates(&self) -> Option<(f64, f64)> {
        let (&(t0, d0, w0), &(t1, d1, w1)) = (self.samples.front()?, self.samples.back()?);
        let secs = t1.duration_since(t0).as_secs_f64();
        if secs >= 0.5 {
            return Some((d1.saturating_sub(d0) as f64 / secs, w1.saturating_sub(w0) as f64 / secs));
        }
        let since = self.copying_since?.elapsed().as_secs_f64();
        (since > 0.5).then(|| (self.done as f64 / since, self.bytes_moved() as f64 / since))
    }

    /// Bytes written per second.
    fn speed(&self) -> Option<f64> {
        self.rates().map(|(_, w)| w)
    }

    /// Progress units per second, for the time left.
    fn unit_rate(&self) -> Option<f64> {
        self.rates().map(|(d, _)| d)
    }

    fn elapsed(&self) -> Duration {
        self.copying_since.map(|t| t.elapsed()).unwrap_or_default()
    }
}

/// The whole run fitted into the chart's bars (nothing for runs too short to chart).
fn overview(history: &[f64]) -> Vec<f64> {
    if history.len() < 4 {
        return Vec::new();
    }
    (0..BARS)
        .map(|i| {
            let start = i * history.len() / BARS;
            let end = ((i + 1) * history.len() / BARS).clamp(start + 1, history.len());
            let chunk = &history[start..end];
            chunk.iter().sum::<f64>() / chunk.len() as f64
        })
        .collect()
}

fn normalized(values: Vec<f64>) -> ModelRc<f32> {
    let max = values.iter().copied().fold(0.0, f64::max);
    let bars: Vec<f32> = values.iter().map(|v| if max > 0.0 { (v / max * 0.92) as f32 } else { 0.0 }).collect();
    ModelRc::new(VecModel::from(bars))
}

fn success_title(action: &str) -> &'static str {
    match action {
        "Write to drive" => "Drive written",
        "Create image" => "Image created",
        "Clone drive" => "Drive cloned",
        "Wipe drive" => "Drive wiped",
        "Write zeros" => "Zeros written",
        _ => "Copy finished",
    }
}

fn drive_glyph(d: &Drive) -> i32 {
    match d.kind {
        DriveKind::Usb => 1,
        DriveKind::Sd => 2,
        DriveKind::Ssd | DriveKind::Hdd => 3,
        DriveKind::Virtual => 4,
    }
}

fn drive_summary(d: &Drive) -> String {
    [fmt::bytes(d.size), d.bus.clone(), d.path.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
}

/// What the confirmation dialog lists about a drive about to be erased.
fn erase_detail(drive: &Drive) -> String {
    let mut detail = [drive.path.as_str(), drive.bus.as_str(), &fmt::bytes(drive.size)]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ");
    if !drive.volumes.is_empty() {
        detail.push_str(&format!("\nHolds: {}", drive.volumes.join(", ")));
    }
    detail
}

/// Subtitle and tag for an image file on the source card.
fn file_summary(path: &Path, info: &ImageInfo) -> (String, String) {
    let place = folder(path);
    if let Some(smart) = info.smart {
        return (
            format!("{} of data from a {} drive · {place}", fmt::bytes(smart.used), fmt::bytes(smart.disk_size)),
            "SMART IMAGE".to_owned(),
        );
    }
    let tag = match info.format {
        ImageFormat::Raw => return (format!("{} · {place}", fmt::bytes(info.size)), String::new()),
        ImageFormat::Gzip => "GZIP",
        ImageFormat::Xz => "XZ",
        ImageFormat::Zstd => "ZSTD",
        ImageFormat::Zip => "ZIP",
        ImageFormat::Bzip2 => "BZIP2",
        ImageFormat::Lz4 => "LZ4",
        ImageFormat::Lzma => "LZMA",
        ImageFormat::SevenZ => "7-ZIP",
        ImageFormat::Tar => "TAR",
        ImageFormat::Dmg => "DMG",
        ImageFormat::Vhd => "VHD",
        ImageFormat::Vhdx => "VHDX",
        ImageFormat::Vmdk => "VMDK",
        ImageFormat::Qcow2 => "QCOW2",
    };
    let size = match info.uncompressed {
        Some(full) => format!("{} → {} unpacked", fmt::bytes(info.size), fmt::bytes(full)),
        None => fmt::bytes(info.size),
    };
    (format!("{size} · {place}"), tag.to_owned())
}

fn endpoint_name(source: &Source) -> String {
    match source {
        Source::File { path, .. } => file_name(path),
        Source::Drive(d) => d.name.clone(),
        Source::Zeros => "Zeros".to_owned(),
        Source::None => String::new(),
    }
}

fn target_name(target: &Target) -> String {
    match target {
        Target::File(path) => file_name(path),
        Target::Drive(d) => d.name.clone(),
        Target::None => String::new(),
    }
}

/// Tells two drives with the same name apart by their device names.
fn same_names(from: String, to: String, source: &Drive, target: &Target) -> (String, String) {
    match target {
        Target::Drive(t) if from == to => {
            (format!("{from} ({})", short_path(&source.path)), format!("{to} ({})", short_path(&t.path)))
        }
        _ => (from, to),
    }
}

/// "/dev/sdb" → "sdb"
fn short_path(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// "~/Downloads"
fn folder(path: &Path) -> String {
    path.parent().map(|p| tilde(&p.to_string_lossy())).unwrap_or_default()
}

fn tilde(path: &str) -> String {
    match std::env::home_dir().map(|h| h.to_string_lossy().into_owned()) {
        Some(home) if !home.is_empty() && path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_owned(),
    }
}

/// Shortens long paths in the command preview (the copy button copies the real thing).
fn display_value(key: &str, value: &str) -> String {
    if key != "if" && key != "of" && key != "_" {
        return value.to_owned();
    }
    let value = tilde(value);
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 46 {
        return value;
    }
    let head: String = chars[..16].iter().collect();
    let tail: String = chars[chars.len() - 27..].iter().collect();
    format!("{head}…{tail}")
}

/// Opens a web page in the default browser.
fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    // Through the desktop's own Explorer, so the browser doesn't start as administrator the
    // way DD-GUI runs (and no cmd.exe, which would take `&` in a URL as a command separator).
    #[cfg(windows)]
    let _ = std::process::Command::new(windows_explorer()).arg(url).spawn();
}

fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer wants the quotes around the path only: /select,"C:\My Images\x.img".
        let _ = std::process::Command::new(windows_explorer()).raw_arg(format!("/select,\"{}\"", path.display())).spawn();
    }
}

/// Explorer by its full path: DD-GUI runs as administrator, and a bare "explorer" would be
/// looked for next to dd-gui.exe first (in Downloads, say).
#[cfg(windows)]
fn windows_explorer() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Path::new(&root).join("explorer.exe")
}

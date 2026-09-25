//! Desktop integration on Linux, with no installer: the binary carries its icons and,
//! each time the GUI starts, puts them and a desktop entry in the user's data directory
//! (`$XDG_DATA_HOME`, usually `~/.local/share`). Menus, docks and Wayland taskbars match
//! the window (`app_id` and X11 class `dd-gui`) to `applications/dd-gui.desktop`, and find
//! its icon (`dd-gui`) in `icons/hicolor`. The entry's `Exec` is the running binary, so a
//! moved binary is picked up on its next start.
//!
//! Skipped for root, and when `DD_GUI_NO_DESKTOP_INTEGRATION` is set. A no-op on
//! other systems.

/// Best effort and quiet: nothing here may keep the app from starting.
pub fn integrate() {
    #[cfg(target_os = "linux")]
    linux::integrate();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::SystemTime;

    /// Set to anything, DD-GUI leaves the data directory alone.
    const OPT_OUT: &str = "DD_GUI_NO_DESKTOP_INTEGRATION";

    /// The icon in the hicolor theme, as `<size>x<size>/apps/dd-gui.png`. 1024 px is
    /// left out: no desktop asks for it, and the SVG covers anything bigger.
    const ICONS: [(u32, &[u8]); 8] = [
        (16, include_bytes!("../assets/png/dd-gui-16.png")),
        (24, include_bytes!("../assets/png/dd-gui-24.png")),
        (32, include_bytes!("../assets/png/dd-gui-32.png")),
        (48, include_bytes!("../assets/png/dd-gui-48.png")),
        (64, include_bytes!("../assets/png/dd-gui-64.png")),
        (128, include_bytes!("../assets/png/dd-gui-128.png")),
        (256, include_bytes!("../assets/png/dd-gui-256.png")),
        (512, include_bytes!("../assets/png/dd-gui-512.png")),
    ];

    /// `scalable/apps/dd-gui.svg`
    const SVG: &[u8] = include_bytes!("../assets/icon.svg");

    /// The packaged entry; `Exec` and `TryExec` become the running binary's path.
    const TEMPLATE: &str = include_str!("../packaging/linux/dd-gui.desktop");

    const HEADER: &str = "\
# Written by DD-GUI each time it starts, with Exec pointing at the program, so menus,
# docks and taskbars find it and its icon. To have this file (and the dd-gui icons)
# left alone, set DD_GUI_NO_DESKTOP_INTEGRATION=1.
";

    pub fn integrate() {
        // SAFETY: geteuid has no preconditions and can't fail.
        let root = unsafe { libc::geteuid() } == 0;
        if root || std::env::var_os(OPT_OUT).is_some() {
            return;
        }
        let Some(data) = data_home(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME")) else {
            return;
        };
        let exe = std::env::current_exe().ok().map(running_binary);
        let changes = install(&data, exe.as_deref());
        refresh_caches(&data, changes);
    }

    /// `$XDG_DATA_HOME`, or `$HOME/.local/share` when that's unset, empty or relative
    /// (the XDG Base Directory spec). None without a usable home.
    pub(super) fn data_home(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
        if let Some(dir) = xdg_data_home.map(PathBuf::from).filter(|dir| dir.is_absolute()) {
            return Some(dir);
        }
        let home = PathBuf::from(home?);
        home.is_absolute().then(|| home.join(".local/share"))
    }

    /// `current_exe()` of a binary that was replaced or deleted while it ran ends in
    /// " (deleted)" (it's /proc/self/exe). The file at the original path, if any, is the
    /// one to start next time.
    pub(super) fn running_binary(exe: PathBuf) -> PathBuf {
        if exe.exists() {
            return exe;
        }
        match exe.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
            Some(path) => PathBuf::from(OsStr::from_bytes(path)),
            None => exe,
        }
    }

    /// What `install` wrote.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(super) struct Changes {
        pub icons: bool,
        pub entry: bool,
    }

    /// Puts the icons and, when `exe` is known, the desktop entry under `data`.
    pub(super) fn install(data: &Path, exe: Option<&Path>) -> Changes {
        // A missing base directory is created private (XDG Base Directory spec).
        let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(data);
        let hicolor = data.join("icons/hicolor");
        let mut changes = Changes::default();
        for (size, png) in ICONS {
            changes.icons |= sync(&hicolor.join(format!("{size}x{size}/apps/dd-gui.png")), png);
        }
        changes.icons |= sync(&hicolor.join("scalable/apps/dd-gui.svg"), SVG);
        if let Some(entry) = exe.and_then(desktop_entry) {
            changes.entry = sync(&data.join("applications/dd-gui.desktop"), entry.as_bytes());
        }
        changes
    }

    /// Makes `path` hold `data`, writing only when it differs. Whether it wrote.
    fn sync(path: &Path, data: &[u8]) -> bool {
        if fs::read(path).is_ok_and(|old| old == data) {
            return false;
        }
        write_atomically(path, data).is_ok()
    }

    /// Writes a temporary file next to `path` and renames it over `path`, so nothing
    /// ever sees half a file.
    fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        fs::create_dir_all(dir)?;
        // Hidden and without the .desktop or .png extension, so no menu or icon theme
        // picks it up.
        let mut tmp = OsString::from(".");
        tmp.push(name);
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = dir.join(tmp);
        let _ = fs::remove_file(&tmp);
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .and_then(|mut file| file.write_all(data))
            .and_then(|()| fs::rename(&tmp, path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }

    /// The desktop entry for the binary at `exe`. None when the path can't go in one:
    /// not UTF-8, or with control characters.
    pub(super) fn desktop_entry(exe: &Path) -> Option<String> {
        let exe = exe.to_str().filter(|exe| !exe.chars().any(char::is_control))?;
        let mut entry = String::from(HEADER);
        for line in TEMPLATE.lines() {
            if line.starts_with("Exec=") {
                entry.push_str("Exec=");
                entry.push_str(&escape(&exec_arg(exe)));
                entry.push_str(" %f");
            } else if line.starts_with("TryExec=") {
                entry.push_str("TryExec=");
                entry.push_str(&escape(exe));
            } else {
                entry.push_str(line);
            }
            entry.push('\n');
        }
        Some(entry)
    }

    /// `arg` as one argument of an Exec line (Desktop Entry spec, "The Exec key"):
    /// `%` doubled, and quoted when it has a reserved character. (GNOME won't load an
    /// entry whose program path has a `%` in it at all, but other desktops will.)
    pub(super) fn exec_arg(arg: &str) -> String {
        const RESERVED: &[char] = &[
            ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '`', '=', '(', ')',
        ];
        let arg = arg.replace('%', "%%");
        if !arg.contains(RESERVED) {
            return arg;
        }
        let mut quoted = String::with_capacity(arg.len() + 4);
        quoted.push('"');
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    }

    /// A string value in the desktop file's own escaping, which readers undo first.
    /// Only `\` needs it here: values never hold control characters, and no value
    /// written here starts with a space.
    fn escape(value: &str) -> String {
        value.replace('\\', "\\\\")
    }

    /// Tells the desktop about the changes: refreshes the MIME cache of the menu
    /// entries and the icon cache, like xdg-utils does. Detached and silent.
    fn refresh_caches(data: &Path, changes: Changes) {
        if changes.entry {
            spawn_detached(
                Command::new("update-desktop-database")
                    .arg("-q")
                    .arg(data.join("applications")),
            );
        }
        if changes.icons {
            let hicolor = data.join("icons/hicolor");
            // GTK ignores an icon cache that is older than the theme directory, so a
            // stale cache can't hide the new icons even if it isn't rebuilt below.
            let _ = fs::File::open(&hicolor).and_then(|dir| dir.set_modified(SystemTime::now()));
            // Only rebuild a cache that is already there: a new cache would hide icons
            // that other programs add later without updating it.
            if hicolor.join("icon-theme.cache").exists() {
                spawn_detached(
                    Command::new("gtk-update-icon-cache")
                        .args(["-q", "-f", "-t"])
                        .arg(&hicolor),
                );
            }
        }
    }

    /// Starts `command` (if it's installed) with no input or output, in its own process
    /// group, and doesn't wait for it: a thread reaps it when it's done.
    fn spawn_detached(command: &mut Command) {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        if let Ok(mut child) = child {
            let _ = std::thread::Builder::new()
                .name("desktop-cache".into())
                .spawn(move || child.wait());
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::{Changes, data_home, desktop_entry, exec_arg, install, running_binary};
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh directory standing in for `$XDG_DATA_HOME`; removed afterwards.
    struct DataHome(PathBuf);

    impl DataHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("dd-gui-desktop-test-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            DataHome(dir)
        }

        fn entry(&self) -> String {
            fs::read_to_string(self.0.join("applications/dd-gui.desktop")).unwrap()
        }

        fn files(&self) -> Vec<String> {
            files_under(&self.0)
        }
    }

    /// Every file under `root` (none if it doesn't exist), relative to it and sorted.
    fn files_under(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            let Ok(entries) = fs::read_dir(dir) else { return };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    out.push(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    impl Drop for DataHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The value of `key` in the entry's [Desktop Entry] group.
    fn value<'a>(entry: &'a str, key: &str) -> &'a str {
        let mut values = entry
            .lines()
            .filter_map(|line| line.strip_prefix(key)?.strip_prefix('='));
        let value = values.next().unwrap_or_else(|| panic!("no {key} in:\n{entry}"));
        assert!(values.next().is_none(), "{key} twice in:\n{entry}");
        value
    }

    /// Reads an Exec value back the way desktops do (Desktop Entry spec): the file's
    /// escapes, then field codes (`%f` becomes "FILE"), then the quoting.
    fn parse_exec(value: &str) -> Vec<String> {
        let mut unescaped = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            unescaped.push(match c {
                '\\' => match chars.next() {
                    Some('\\') => '\\',
                    Some('s') => ' ',
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('r') => '\r',
                    other => panic!("bad escape \\{other:?} in {value:?}"),
                },
                c => c,
            });
        }
        let mut expanded = String::new();
        let mut chars = unescaped.chars();
        while let Some(c) = chars.next() {
            match c {
                '%' => match chars.next() {
                    Some('%') => expanded.push('%'),
                    Some('f') => expanded.push_str("FILE"),
                    other => panic!("field code %{other:?} in {value:?}"),
                },
                c => expanded.push(c),
            }
        }
        let mut args = Vec::new();
        let mut chars = expanded.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c == ' ' {
                chars.next();
                continue;
            }
            let mut arg = String::new();
            if c == '"' {
                chars.next();
                loop {
                    match chars.next().expect("unterminated quote") {
                        '"' => break,
                        '\\' => {
                            let next = chars.next().expect("dangling backslash");
                            assert!(matches!(next, '"' | '`' | '$' | '\\'), "\\{next} in quotes");
                            arg.push(next);
                        }
                        c => arg.push(c),
                    }
                }
                assert!(
                    matches!(chars.peek(), None | Some(' ')),
                    "junk after quotes in {value:?}"
                );
            } else {
                while let Some(&c) = chars.peek() {
                    if c == ' ' {
                        break;
                    }
                    assert!(
                        !"\"'\\><~|&;$*?#`=()\t\n".contains(c),
                        "reserved {c:?} unquoted in {value:?}"
                    );
                    arg.push(c);
                    chars.next();
                }
            }
            args.push(arg);
        }
        args
    }

    const WROTE_ALL: Changes = Changes {
        icons: true,
        entry: true,
    };
    const WROTE_ICONS: Changes = Changes {
        icons: true,
        entry: false,
    };
    const WROTE_ENTRY: Changes = Changes {
        icons: false,
        entry: true,
    };
    const WROTE_NOTHING: Changes = Changes {
        icons: false,
        entry: false,
    };

    const ALL_FILES: [&str; 10] = [
        "applications/dd-gui.desktop",
        "icons/hicolor/128x128/apps/dd-gui.png",
        "icons/hicolor/16x16/apps/dd-gui.png",
        "icons/hicolor/24x24/apps/dd-gui.png",
        "icons/hicolor/256x256/apps/dd-gui.png",
        "icons/hicolor/32x32/apps/dd-gui.png",
        "icons/hicolor/48x48/apps/dd-gui.png",
        "icons/hicolor/512x512/apps/dd-gui.png",
        "icons/hicolor/64x64/apps/dd-gui.png",
        "icons/hicolor/scalable/apps/dd-gui.svg",
    ];

    #[test]
    fn installs_the_icons_and_the_entry() {
        let home = DataHome::new();
        let exe = Path::new("/opt/dd-gui/dd-gui");
        assert_eq!(install(&home.0, Some(exe)), WROTE_ALL);
        // No temporary files left behind.
        assert_eq!(home.files(), ALL_FILES);

        let png = fs::read(home.0.join("icons/hicolor/48x48/apps/dd-gui.png")).unwrap();
        assert_eq!(png, include_bytes!("../assets/png/dd-gui-48.png"));
        assert!(png.starts_with(b"\x89PNG"));
        let svg = fs::read(home.0.join("icons/hicolor/scalable/apps/dd-gui.svg")).unwrap();
        assert_eq!(svg, include_bytes!("../assets/icon.svg"));

        let entry = home.entry();
        assert!(entry.lines().any(|line| line == "[Desktop Entry]"));
        assert_eq!(value(&entry, "Exec"), "/opt/dd-gui/dd-gui %f");
        assert_eq!(value(&entry, "TryExec"), "/opt/dd-gui/dd-gui");
        assert_eq!(value(&entry, "Type"), "Application");
        assert_eq!(value(&entry, "Name"), "DD-GUI");
        assert_eq!(value(&entry, "GenericName"), "Disk Imager");
        assert!(!value(&entry, "Comment").is_empty());
        assert_eq!(value(&entry, "Icon"), "dd-gui");
        assert_eq!(value(&entry, "StartupWMClass"), "dd-gui");
        assert_eq!(value(&entry, "Categories"), "System;Utility;");
        assert_eq!(value(&entry, "Terminal"), "false");
        let mime: Vec<&str> = value(&entry, "MimeType").split_terminator(';').collect();
        for t in [
            "application/vnd.efi.iso",
            "application/x-cd-image",
            "application/vnd.efi.img",
            "application/x-raw-disk-image",
            "application/x-raw-disk-image-xz-compressed",
            "application/gzip",
            "application/x-xz",
            "application/zstd",
            "application/zip",
            "application/x-apple-diskimage",
            "application/x-vhd-disk",
            "application/x-vhdx-disk",
            "application/x-vmdk-disk",
            "application/x-qemu-disk",
        ] {
            assert!(mime.contains(&t), "{t} missing from MimeType");
        }
    }

    #[test]
    fn a_second_start_writes_nothing() {
        let home = DataHome::new();
        let exe = Path::new("/opt/dd-gui/dd-gui");
        install(&home.0, Some(exe));
        let desktop = home.0.join("applications/dd-gui.desktop");
        let icon = home.0.join("icons/hicolor/256x256/apps/dd-gui.png");
        let stamps = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
        let before = (stamps(&desktop), stamps(&icon));
        assert_eq!(install(&home.0, Some(exe)), WROTE_NOTHING);
        assert_eq!((stamps(&desktop), stamps(&icon)), before);
        assert_eq!(home.files(), ALL_FILES);
    }

    #[test]
    fn moving_the_binary_updates_exec() {
        let home = DataHome::new();
        install(&home.0, Some(Path::new("/home/me/Downloads/dd-gui")));
        let moved = Path::new("/home/me/.local/bin/dd-gui");
        assert_eq!(install(&home.0, Some(moved)), WROTE_ENTRY);
        assert_eq!(value(&home.entry(), "Exec"), "/home/me/.local/bin/dd-gui %f");
        assert_eq!(value(&home.entry(), "TryExec"), "/home/me/.local/bin/dd-gui");
    }

    #[test]
    fn broken_or_edited_files_are_rewritten() {
        let home = DataHome::new();
        let exe = Path::new("/opt/dd-gui/dd-gui");
        install(&home.0, Some(exe));
        let icon = home.0.join("icons/hicolor/32x32/apps/dd-gui.png");
        fs::write(&icon, b"").unwrap();
        fs::write(
            home.0.join("applications/dd-gui.desktop"),
            "[Desktop Entry]\nExec=elsewhere\n",
        )
        .unwrap();
        assert_eq!(install(&home.0, Some(exe)), WROTE_ALL);
        assert_eq!(fs::read(&icon).unwrap(), include_bytes!("../assets/png/dd-gui-32.png"));
        assert_eq!(value(&home.entry(), "Exec"), "/opt/dd-gui/dd-gui %f");
    }

    #[test]
    fn without_a_usable_exe_path_only_the_icons_go_in() {
        use std::os::unix::ffi::OsStringExt;
        let home = DataHome::new();
        assert_eq!(install(&home.0, None), WROTE_ICONS);
        let not_utf8 = PathBuf::from(OsString::from_vec(b"/opt/\xffdd/dd-gui".to_vec()));
        assert_eq!(install(&home.0, Some(&not_utf8)), WROTE_NOTHING);
        assert_eq!(install(&home.0, Some(Path::new("/opt/a\nb/dd-gui"))), WROTE_NOTHING);
        assert_eq!(install(&home.0, Some(Path::new("/opt/a\u{1b}b/dd-gui"))), WROTE_NOTHING);
        assert!(!home.0.join("applications").exists());
    }

    #[test]
    fn exec_quoting() {
        // Examples straight from the Exec rules.
        for (path, exec) in [
            ("/usr/bin/dd-gui", "/usr/bin/dd-gui %f"),
            ("/home/me/My Apps/dd-gui", r#""/home/me/My Apps/dd-gui" %f"#),
            ("/home/me/100%/dd-gui", "/home/me/100%%/dd-gui %f"),
            ("/home/me/a=b/dd-gui", r#""/home/me/a=b/dd-gui" %f"#),
            ("/home/me/~/dd-gui", r#""/home/me/~/dd-gui" %f"#),
            // A literal $ is written \\$ and a literal backslash \\\\ (the spec's own examples).
            ("/home/me/$HOME/dd-gui", r#""/home/me/\\$HOME/dd-gui" %f"#),
            (r"/home/me/back\slash/dd-gui", r#""/home/me/back\\\\slash/dd-gui" %f"#),
            (r#"/home/me/"quoted"/dd-gui"#, r#""/home/me/\\"quoted\\"/dd-gui" %f"#),
            ("/home/me/`tick`/dd-gui", r#""/home/me/\\`tick\\`/dd-gui" %f"#),
            ("/home/me/it's/dd-gui", r#""/home/me/it's/dd-gui" %f"#),
            ("/home/me/Programme/dd-gui (1)", r#""/home/me/Programme/dd-gui (1)" %f"#),
            ("/home/josé/приложения/dd-gui", "/home/josé/приложения/dd-gui %f"),
        ] {
            let entry = desktop_entry(Path::new(path)).unwrap();
            assert_eq!(value(&entry, "Exec"), exec, "for {path}");
            assert_eq!(parse_exec(value(&entry, "Exec")), [path, "FILE"], "for {path}");
            // TryExec is a plain string: only the file's own escaping applies.
            assert_eq!(value(&entry, "TryExec"), path.replace('\\', r"\\"), "for {path}");
        }
        // Every reserved character, and some that aren't, round-trip.
        for c in " \"'\\><~|&;$*?#`=()%!@^[]{},+:".chars() {
            let path = format!("/tmp/a{c}b{c}/dd-gui");
            let entry = desktop_entry(Path::new(&path)).unwrap();
            assert_eq!(parse_exec(value(&entry, "Exec")), [path.as_str(), "FILE"]);
        }
        assert_eq!(exec_arg("plain"), "plain");
        assert_eq!(exec_arg("50%"), "50%%");
        assert_eq!(exec_arg("50% off"), r#""50%% off""#);
    }

    #[test]
    fn the_entry_is_valid() {
        let home = DataHome::new();
        install(&home.0, Some(Path::new("/home/me/My Apps/$dd-gui")));
        let desktop = home.0.join("applications/dd-gui.desktop");
        // desktop-file-utils, when it's installed.
        if let Ok(out) = std::process::Command::new("desktop-file-validate")
            .arg(&desktop)
            .output()
        {
            assert!(
                out.status.success(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let entry = home.entry();
        assert!(entry.starts_with('#'), "starts with the comment that explains it");
        assert!(entry.ends_with('\n'));
    }

    #[test]
    fn the_template_has_one_exec_and_try_exec() {
        let template = include_str!("../packaging/linux/dd-gui.desktop");
        assert_eq!(value(template, "Exec"), "dd-gui %f");
        assert_eq!(value(template, "TryExec"), "dd-gui");
        assert_eq!(value(template, "Icon"), "dd-gui");
        assert_eq!(value(template, "StartupWMClass"), "dd-gui");
        // Actions would have Exec lines of their own, which desktop_entry doesn't handle.
        assert_eq!(template.lines().filter(|line| line.starts_with('[')).count(), 1);
    }

    #[test]
    fn deleted_suffix() {
        let home = DataHome::new();
        let gone = home.0.join("dd-gui");
        assert_eq!(
            running_binary(PathBuf::from(format!("{} (deleted)", gone.display()))),
            gone
        );
        // A file that's really called that stays as it is.
        let odd = home.0.join("odd (deleted)");
        fs::write(&odd, b"").unwrap();
        assert_eq!(running_binary(odd.clone()), odd);
        assert_eq!(running_binary(gone.clone()), gone);
    }

    #[test]
    fn data_home_follows_xdg() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(data_home(os("/data"), os("/home/me")), Some(PathBuf::from("/data")));
        assert_eq!(
            data_home(None, os("/home/me")),
            Some(PathBuf::from("/home/me/.local/share"))
        );
        // Empty or relative values are ignored (XDG Base Directory spec).
        assert_eq!(
            data_home(os(""), os("/home/me")),
            Some(PathBuf::from("/home/me/.local/share"))
        );
        assert_eq!(
            data_home(os("data"), os("/home/me")),
            Some(PathBuf::from("/home/me/.local/share"))
        );
        assert_eq!(data_home(None, None), None);
        assert_eq!(data_home(os(""), os("")), None);
        assert_eq!(data_home(None, os("me")), None);
    }

    #[test]
    fn a_missing_data_directory_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = DataHome::new();
        let data = home.0.join("new/share");
        assert_eq!(install(&data, Some(Path::new("/opt/dd-gui/dd-gui"))), WROTE_ALL);
        for dir in [home.0.join("new"), data.clone()] {
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        assert_eq!(files_under(&data), ALL_FILES);
    }

    /// `integrate()` as the app calls it, reading the environment. The environment is per
    /// process, so this test binary runs again as a child, with `HOME` and `XDG_DATA_HOME`
    /// in a temporary directory: it never touches the real home.
    #[test]
    fn integrate_follows_the_environment() {
        const CHILD: &str = "DD_GUI_DESKTOP_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            super::integrate();
            return;
        }
        // SAFETY: geteuid has no preconditions and can't fail.
        if unsafe { libc::geteuid() } == 0 {
            return; // integrate() leaves root alone
        }
        let home = DataHome::new();
        let exe = std::env::current_exe().unwrap();
        let this_test = format!(
            "{}::integrate_follows_the_environment",
            module_path!().split_once("::").unwrap().1
        );
        let integrate = |xdg_data_home: Option<&str>, opt_out: bool| {
            let mut child = std::process::Command::new(&exe);
            child
                .args(["--exact", &this_test, "--test-threads=1", "--quiet"])
                .env(CHILD, "1")
                .env("HOME", &home.0)
                .env_remove("XDG_DATA_HOME")
                .env_remove("DD_GUI_NO_DESKTOP_INTEGRATION")
                // No update-desktop-database or gtk-update-icon-cache: nothing outlives the child.
                .env("PATH", home.0.join("no-tools"));
            if let Some(dir) = xdg_data_home {
                child.env("XDG_DATA_HOME", home.0.join(dir));
            }
            if opt_out {
                child.env("DD_GUI_NO_DESKTOP_INTEGRATION", "1");
            }
            let out = child.output().unwrap();
            let log = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{log}");
            assert!(log.contains("1 passed"), "the child didn't run {this_test}: {log}");
        };

        integrate(Some("xdg"), true);
        integrate(None, true);
        assert_eq!(home.files(), Vec::<String>::new(), "DD_GUI_NO_DESKTOP_INTEGRATION");

        integrate(Some("xdg"), false);
        assert_eq!(files_under(&home.0.join("xdg")), ALL_FILES, "XDG_DATA_HOME");
        let entry = fs::read_to_string(home.0.join("xdg/applications/dd-gui.desktop")).unwrap();
        assert_eq!(parse_exec(value(&entry, "Exec")), [exe.to_str().unwrap(), "FILE"]);
        assert_eq!(files_under(&home.0.join(".local")), Vec::<String>::new());

        integrate(None, false);
        assert_eq!(
            files_under(&home.0.join(".local/share")),
            ALL_FILES,
            "HOME/.local/share"
        );
    }
}

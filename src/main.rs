// No console window for the GUI on Windows (dd mode re-attaches to the parent console).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod desktop;
mod drives;
mod engine;
mod fmt;
mod job;
mod plan;
mod smart;
mod worker;

fn main() {
    match std::env::args_os().nth(1) {
        // `dd-gui dd …` is the bundled dd. The GUI runs its (elevated) worker this way too.
        Some(arg) if arg == "dd" => std::process::exit(worker::main()),
        // `dd-gui copy …` is DD-GUI's own copier: smart copies and compressed images.
        Some(arg) if arg == "copy" => std::process::exit(engine::main()),
        // `dd-gui drives`: the drive list as JSON (what the GUI shows; handy for scripts and tests).
        Some(arg) if arg == "drives" => std::process::exit(match drives::list() {
            Ok(list) => {
                println!("{}", serde_json::to_string_pretty(&list).unwrap_or_default());
                0
            }
            Err(err) => {
                eprintln!("dd-gui: {err}");
                1
            }
        }),
        _ => {}
    }
    if let Err(err) = app::run() {
        eprintln!("dd-gui: {err}");
        std::process::exit(1);
    }
}

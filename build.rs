use std::env;
use std::io;
use std::path::Path;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile the Slint UI");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Raw disk access on Windows needs admin rights, so the Windows build asks for
        // them up front through its manifest (like Rufus and Win32 Disk Imager do).
        use embed_manifest::manifest::ExecutionLevel;
        embed_manifest::embed_manifest(
            embed_manifest::new_manifest("dd-gui").requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("failed to embed the Windows manifest");
        // It runs as administrator, often from Downloads: the DLLs it links against come
        // from System32 only, never from the folder it sits in (Windows 10 1607 and later).
        if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        }

        embed_windows_resources();
    }
    println!("cargo:rerun-if-changed=build.rs");
}

/// Gives dd-gui.exe its icon and version info (Explorer, the taskbar, Task Manager).
///
/// winresource writes them into an .rc file and runs a resource compiler:
/// - rc.exe from the Windows SDK for MSVC builds on Windows (found through the registry);
/// - windres for GNU targets: MinGW's own on Windows, `x86_64-w64-mingw32-windres` when
///   cross-compiling (or `WINDRES`);
/// - `llvm-rc` (or `RC_PATH`) for MSVC targets on other systems.
///
/// No manifest here: embed_manifest above makes the one and only.
///
/// A Windows build gets the icon or fails. The one exception is a non-Windows host with no
/// resource compiler at all, e.g. `cargo check --target x86_64-pc-windows-msvc` on Linux,
/// which only warns: build scripts can't tell `cargo check` from `cargo build`, so the link
/// is poisoned instead, and a real build stops at the link with a message naming the icon.
fn embed_windows_resources() {
    let root = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let icon = Path::new(&root).join("assets").join("dd-gui.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    for var in ["WINDRES", "AR", "RC_PATH", "CROSS_COMPILE"] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("the project path isn't valid UTF-8"))
        .set("ProductName", "DD-GUI")
        .set("FileDescription", "DD-GUI")
        .set("InternalName", "dd-gui")
        .set("OriginalFilename", "dd-gui.exe");
    // FileVersion and ProductVersion come from CARGO_PKG_VERSION; CompanyName stays unset.

    match res.compile() {
        Ok(()) => {}
        // No resource compiler installed (not one that ran and failed), on a host where
        // this is likely just a check. A Windows host always has one: rc.exe comes with
        // the SDK that MSVC links against, windres with MinGW.
        Err(err) if err.kind() == io::ErrorKind::NotFound && !cfg!(windows) => {
            let out_dir = env::var("OUT_DIR").expect("cargo sets OUT_DIR");
            let poison = Path::new(&out_dir).join("ERROR-dd-gui.exe-has-no-icon-no-resource-compiler-see-build.rs");
            println!(
                "cargo:warning=no resource compiler for dd-gui.exe's icon and version info ({err}), so linking \
                 it will fail; checking is fine. Install MinGW's windres (GNU targets) or llvm-rc (MSVC targets), \
                 or point WINDRES or RC_PATH at one."
            );
            println!("cargo:rustc-link-arg-bins={}", poison.display());
            // It doesn't exist, so cargo runs this script again next time, and finds the
            // resource compiler once there is one.
            println!("cargo:rerun-if-changed={}", poison.display());
        }
        Err(err) => panic!("failed to embed the Windows icon and version info in dd-gui.exe: {err}"),
    }
}

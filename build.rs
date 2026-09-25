fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile the Slint UI");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Raw disk access on Windows needs admin rights, so the Windows build asks for
        // them up front through its manifest (like Rufus and Win32 Disk Imager do).
        use embed_manifest::manifest::ExecutionLevel;
        embed_manifest::embed_manifest(
            embed_manifest::new_manifest("DD-GUI")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("failed to embed the Windows manifest");

        embed_windows_resources();
    }
    println!("cargo:rerun-if-changed=build.rs");
}

/// Gives DD-GUI.exe its icon and version info (Explorer's Details tab, Task Manager).
///
/// This needs a resource compiler: rc.exe from the Windows SDK when building on Windows
/// with MSVC, windres with MinGW, or llvm-rc (or `RC_PATH`) when cross-compiling. Without
/// one, e.g. a plain `cargo check --target x86_64-pc-windows-msvc` on Linux, the build
/// carries on with a warning and the exe simply has no icon.
///
/// The manifest stays with embed_manifest above: no manifest is set here, so the exe
/// ends up with exactly one.
fn embed_windows_resources() {
    let root = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let icon = std::path::Path::new(&root).join("assets").join("DD-GUI.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("the project path isn't valid UTF-8"))
        .set("ProductName", "DD-GUI")
        .set("FileDescription", "DD-GUI")
        .set("InternalName", "DD-GUI")
        .set("OriginalFilename", "DD-GUI.exe");
    // FileVersion and ProductVersion come from CARGO_PKG_VERSION; CompanyName stays unset.

    if let Err(err) = res.compile() {
        // A Windows host building with MSVC always has rc.exe (it comes with the SDK the
        // linker needs), so a failure there is real and a release must not quietly ship
        // without its icon.
        let native_msvc = cfg!(windows) && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
        if native_msvc {
            panic!("failed to embed the Windows icon and version info: {err}");
        }
        println!(
            "cargo:warning=no Windows icon or version info in this build: {err} \
             (install llvm-rc or MinGW's windres to embed them when cross-compiling)"
        );
    }
}

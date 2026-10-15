use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-env-changed=RUSTBACKUP_GOOGLE_OAUTH_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=RUSTBACKUP_GOOGLE_OAUTH_CLIENT_SECRET");
    println!("cargo:rerun-if-env-changed=RUSTBACKUP_GOOGLE_OAUTH_ENVIRONMENT");

    let mut attributes = tauri_build::Attributes::new();
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir =
            env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the manifest directory");
        let icon_path = Path::new(&manifest_dir).join("icons/icon.ico");
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new().window_icon_path(icon_path));
    }

    tauri_build::try_build(attributes).expect("failed to build Tauri application metadata");
}

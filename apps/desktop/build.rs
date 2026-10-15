use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-env-changed=RUSTBACKUP_GOOGLE_OAUTH_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=RUSTBACKUP_GOOGLE_OAUTH_CLIENT_SECRET");
    println!("cargo:rerun-if-env-changed=RUSTBACKUP_GOOGLE_OAUTH_ENVIRONMENT");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir =
            env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the manifest directory");
        let target_triple = env::var("TARGET").expect("Cargo sets the target triple");
        let sidecar =
            Path::new(&manifest_dir).join(format!("binaries/rustbackup-agent-{target_triple}.exe"));
        println!("cargo:rerun-if-changed={}", sidecar.display());
        if !sidecar.is_file() {
            let mut config = env::var("TAURI_CONFIG")
                .ok()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                .filter(serde_json::Value::is_object)
                .unwrap_or_else(|| serde_json::json!({}));
            config["bundle"]["externalBin"] = serde_json::json!([]);
            let config = serde_json::to_string(&config).expect("Tauri config serializes");
            env::set_var("TAURI_CONFIG", &config);
            println!("cargo:rustc-env=TAURI_CONFIG={config}");
            println!("cargo:warning=Agent sidecar is not staged; cargo builds omit the bundle sidecar. The Windows packaging script stages it before bundling.");
        }
    }

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

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let mut attributes = tauri_build::Attributes::new();
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let output = env::var_os("OUT_DIR").expect("OUT_DIR is provided by Cargo");
        let icon_path = Path::new(&output).join("rustbackup-app.ico");
        fs::write(&icon_path, windows_icon()).expect("write generated Windows app icon");
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new().window_icon_path(icon_path));
    }

    tauri_build::try_build(attributes).expect("failed to build Tauri application metadata");
}

fn windows_icon() -> Vec<u8> {
    const WIDTH: usize = 32;
    const HEIGHT: usize = 32;
    const XOR_BYTES: usize = WIDTH * HEIGHT * 4;
    const MASK_BYTES: usize = WIDTH.div_ceil(32) * 4 * HEIGHT;
    const DIB_BYTES: usize = 40 + XOR_BYTES + MASK_BYTES;

    let mut icon = Vec::with_capacity(22 + DIB_BYTES);
    icon.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    icon.extend_from_slice(&[WIDTH as u8, HEIGHT as u8, 0, 0]);
    icon.extend_from_slice(&1u16.to_le_bytes());
    icon.extend_from_slice(&32u16.to_le_bytes());
    icon.extend_from_slice(&(DIB_BYTES as u32).to_le_bytes());
    icon.extend_from_slice(&22u32.to_le_bytes());

    icon.extend_from_slice(&40u32.to_le_bytes());
    icon.extend_from_slice(&(WIDTH as i32).to_le_bytes());
    icon.extend_from_slice(&((HEIGHT * 2) as i32).to_le_bytes());
    icon.extend_from_slice(&1u16.to_le_bytes());
    icon.extend_from_slice(&32u16.to_le_bytes());
    icon.extend_from_slice(&0u32.to_le_bytes());
    icon.extend_from_slice(&((XOR_BYTES + MASK_BYTES) as u32).to_le_bytes());
    icon.extend_from_slice(&0i32.to_le_bytes());
    icon.extend_from_slice(&0i32.to_le_bytes());
    icon.extend_from_slice(&0u32.to_le_bytes());
    icon.extend_from_slice(&0u32.to_le_bytes());

    let letter = [
        "01110", "10001", "10001", "10001", "11110", "10010", "10001",
    ];
    for bitmap_row in (0..HEIGHT).rev() {
        for x in 0..WIDTH {
            let glyph_x = x.checked_sub(8).filter(|value| *value < 15);
            let glyph_y = bitmap_row.checked_sub(5).filter(|value| *value < 21);
            let is_letter = glyph_x.zip(glyph_y).is_some_and(|(glyph_x, glyph_y)| {
                let column = glyph_x / 3;
                let row = glyph_y / 3;
                letter[row].as_bytes()[column] == b'1'
            });
            if is_letter {
                icon.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
            } else {
                icon.extend_from_slice(&[0xd2, 0x69, 0x17, 0xff]);
            }
        }
    }
    icon.resize(icon.len() + MASK_BYTES, 0);
    icon
}

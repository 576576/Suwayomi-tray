use std::env;
use std::fs;
use std::path::Path;

fn main() {
    // Tauri 的资源/配置生成（图标清单、能力、CSP 等）
    tauri_build::build();

    // 编译期解码托盘图标：格式错误在构建阶段就失败，而不是运行时 panic。
    // 解码后的 RGBA 原样落到 OUT_DIR，运行时直接包成 tauri Image。
    embed_tray_icon("icons/tray.png");
}

/// 读取并解码托盘 PNG，把 RGBA 字节 + 宽高常量写进 OUT_DIR，供 main.rs 用
/// `include!` 在编译期内嵌。任何一步失败都 `panic!`：把资源错误从运行时崩溃
/// 提前成构建错误。
fn embed_tray_icon(rel: &str) {
    let icon_path = Path::new(rel);
    let data = fs::read(icon_path)
        .unwrap_or_else(|e| panic!("读取托盘图标 {rel} 失败: {e}（请确认 {rel} 存在）"));

    let mut reader = png::Decoder::new(data.as_slice())
        .read_info()
        .unwrap_or_else(|e| panic!("托盘图标 {rel} 不是合法 PNG: {e}"));
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("解码托盘图标 {rel} 失败: {e}"));

    let (color, depth, w, h) = (info.color_type, info.bit_depth, info.width, info.height);
    let rgba: Vec<u8> = match (color, depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buf,
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            let (chunks, _rest) = buf.as_chunks::<3>();
            let mut out = Vec::with_capacity(chunks.len() * 4);
            for [r, g, b] in chunks {
                out.extend_from_slice(&[*r, *g, *b, 255]);
            }
            out
        }
        other => panic!(
            "托盘图标格式不受支持: {:?} / {:?}（仅支持 8-bit RGB / RGBA）",
            other.0, other.1
        ),
    };

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR 未设置（cargo 一定会提供）");
    fs::write(format!("{out_dir}/tray_icon.rgba"), &rgba)
        .expect("写入解码后的托盘图标字节失败");

    let gen = format!(
        "pub const TRAY_ICON_RGBA: &[u8] = \
         include_bytes!(concat!(env!(\"OUT_DIR\"), \"/tray_icon.rgba\"));\n\
         pub const TRAY_ICON_W: u32 = {w};\n\
         pub const TRAY_ICON_H: u32 = {h};\n"
    );
    fs::write(format!("{out_dir}/tray_icon.rs"), gen).expect("写入托盘图标常量失败");

    // 图标变化时需要重新生成
    println!("cargo:rerun-if-changed={rel}");
}

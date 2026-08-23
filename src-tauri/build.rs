fn main() {
    // 把 ui/emitter.js 嵌成 Rust 字符串（单一事实源在 ui/，无构建步骤）。
    let src = std::fs::read_to_string("../ui/emitter.js").expect("read ui/emitter.js");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(
        std::path::Path::new(&out_dir).join("emitter_src.rs"),
        format!("pub const EMITTER_JS: &str = {:?};", src),
    )
    .expect("write emitter_src.rs");
    println!("cargo:rerun-if-changed=../ui/emitter.js");
    tauri_build::build()
}

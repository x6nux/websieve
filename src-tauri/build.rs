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

    ensure_frontend_dist();
    tauri_build::build()
}

/// `frontendDist` 指向的目录不存在时，tauri 的代码生成宏会直接 panic
/// （实测："The `frontendDist` configuration is set to ... but this path
/// doesn't exist"）。而 scripts/e2e.sh 是裸 cargo build，不跑 npm ——
/// 干净克隆上必然撞上。
///
/// 因此写一个占位页兜底：控制窗口能开、能看出「前端没构建」，
/// 传输链路（E2E 真正验的东西）完全不受影响。
///
/// 注意**不要**给 ../ui/dist 加 rerun-if-changed：那会让每次前端构建都
/// 触发整个 app crate 重编译。Tauri 的资产嵌入本来就在 tauri_build::build()
/// 内部处理依赖追踪。
///
/// ponytail: 占位页是硬编码的一行 HTML，不做模板、不做 i18n。
/// 上限：用户看到中文提示；升级路径：真需要时换成读 ui/placeholder.html。
fn ensure_frontend_dist() {
    let dist = std::path::Path::new("../ui/dist");
    let index = dist.join("index.html");
    if index.exists() {
        return;
    }
    std::fs::create_dir_all(dist).expect("create ui/dist");
    std::fs::write(
        &index,
        "<!doctype html><meta charset=\"utf-8\"><title>websieve</title>\
         <body style=\"background:#16181b;color:#e6e8ea;font:13px system-ui;padding:24px\">\
         前端尚未构建。运行 <code>npm --prefix ui run build</code> 后重新编译。</body>",
    )
    .expect("write placeholder index.html");
    println!(
        "cargo:warning=ui/dist 缺失，已写入占位页；\
         跑 `npm --prefix ui run build` 生成真实前端"
    );
}

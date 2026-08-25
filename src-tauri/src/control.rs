//! 控制窗口的建、显、隐（设计文档 §11.3 / §12）。
//!
//! 与传输窗口 `main` 的对照 —— 这两个窗口几乎在每一点上都相反：
//!
//! |          | main（传输）              | control（控制）        |
//! |----------|---------------------------|------------------------|
//! | URL      | External（远端服务器页面）| App（本地嵌入资产）    |
//! | 可见性   | 默认隐藏                  | 默认显示               |
//! | 关闭行为 | 不适用（用户看不到）      | 隐藏，不销毁           |
//! | 权限     | 三个 wsieve_* 命令        | 配置与控制命令         |
//! | 信任     | **不可信**（服务器控制）  | 可信（我们打包的）     |
//!
//! 最后一行是全部安全设计的出发点，见 capabilities/ 下两份文件。

use tauri::{Manager, Runtime, WebviewWindow};

pub const LABEL: &str = "control";

/// 打开控制窗口。已存在则显示并聚焦，不重复建。
///
/// 「已存在」的判定必须走 `get_webview_window` 而不是自己记一个 bool ——
/// 窗口可能被别的路径销毁，缓存的标记会撒谎。
///
/// **这条早退分支是可重开性的全部依据**：用户点关闭后窗口只是隐藏（见
/// `build` 里的 `CloseRequested` 拦截），标签仍被占用。若这里改成无条件
/// `build`，第二次打开会撞上 Tauri 的标签唯一性检查并报
/// 「a webview window with label `control` already exists」—— 表现为
/// 「关掉就再也打不开」。`open_is_idempotent` 守住这一点。
pub fn open<R: Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<WebviewWindow<R>> {
    if let Some(w) = app.get_webview_window(LABEL) {
        w.show()?;
        w.unminimize().ok(); // 最小化状态下 show() 不会还原
        w.set_focus()?;
        return Ok(w);
    }
    build(app)
}

fn build<R: Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<WebviewWindow<R>> {
    let window = tauri::webview::WebviewWindowBuilder::new(
        app,
        LABEL,
        // App(...) 而非 External(...)：加载嵌进二进制的 ui/dist。
        // 注意 debug 构建默认走 devUrl，需要 embed-ui feature
        // （= tauri/custom-protocol）才会真的用嵌入资产。见 Cargo.toml。
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("websieve")
    // §11.3：默认 960×640、最小 720×480（规则表需要宽度）
    .inner_size(960.0, 640.0)
    .min_inner_size(720.0, 480.0)
    .visible(true)
    .resizable(true)
    // 深色界面下，白色的启动闪屏很刺眼。与 tokens.css 的 --surface-0 一致。
    .background_color(tauri::window::Color(0x16, 0x18, 0x1b, 0xff))
    .build()?;

    // §12：控制窗口关闭 → 代理继续运行，托盘常驻。
    // 默认行为是销毁窗口；非 macOS 平台上最后一个窗口销毁会终止事件循环，
    // 于是整个代理跟着死。这里改成隐藏。
    //
    // 注意 WindowEvent 是 #[non_exhaustive]，解构必须带 `..`（否则 E0638）。
    // 闭包体只做「转达 prevent_close + 调 hide」，判决本身在 `close_action`
    // 里 —— MockRuntime 的 `run_iteration` 是空实现（实测：mock_runtime.rs
    // 第 1319 行 `fn run_iteration<F>(&mut self, callback: F) {}`），事件循环
    // 在测试里根本转不起来，闭包永远不会被调用。把判决提成纯函数，
    // 「关闭 = 隐藏而非销毁」才真的有测试守着，而不是靠肉眼点一次。
    let handle = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            match close_action() {
                CloseAction::HideAndKeepRunning => {
                    api.prevent_close();
                    if let Err(e) = handle.hide() {
                        tracing::warn!("隐藏控制窗口失败：{e}");
                    }
                }
            }
        }
    });

    Ok(window)
}

/// 控制窗口收到关闭请求时该做什么。
///
/// 只有一个变体，这不是过度设计而是把 §12 的那句话变成可断言的对象：
/// 「控制窗口关闭 → 代理继续运行」。若哪天有人图省事把 `prevent_close`
/// 删掉走默认销毁路径，`closing_hides_instead_of_destroying` 会红。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// 拦下关闭、隐藏窗口、进程与代理照跑。窗口标签仍被占用，
    /// 因此重开必须走 `open` 的早退分支而非 `build`。
    HideAndKeepRunning,
}

fn close_action() -> CloseAction {
    CloseAction::HideAndKeepRunning
}

/// 显隐切换 —— 托盘点击图标时用。
///
/// 逐项 `allow(dead_code)` 而非整模块开：调用方是托盘菜单（Task 12），
/// 此刻还没接上。已有 `toggle_creates_window_when_absent` 覆盖它的行为，
/// 所以这不是「写了没用的代码」，是「用它的那一头还没到」。
#[allow(dead_code)]
pub fn toggle<R: Runtime>(app: &tauri::AppHandle<R>) {
    match app.get_webview_window(LABEL) {
        Some(w) => {
            let visible = w.is_visible().unwrap_or(false);
            let focused = w.is_focused().unwrap_or(false);
            // 可见但没聚焦时，用户想要的多半是「拿到前台」而不是「藏起来」
            let r = if visible && focused {
                w.hide()
            } else {
                w.show().and_then(|_| w.set_focus())
            };
            if let Err(e) = r {
                tracing::warn!("切换控制窗口失败：{e}");
            }
        }
        None => {
            if let Err(e) = open(app) {
                tracing::error!("打开控制窗口失败：{e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};

    fn mock_app() -> tauri::App<MockRuntime> {
        mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app")
    }

    /// 「关掉就再也打不开」是 Tauri 上最常见的坑：标签是全局唯一的，
    /// 而我们的关闭策略是隐藏（标签不释放），于是第二次打开必须走
    /// show/focus 而不是 build。这个测试直接调两次 `open`。
    ///
    /// 去掉 `open` 里的 `get_webview_window` 早退分支，这条立刻变红
    /// （Tauri 报标签重复）。
    #[test]
    fn open_is_idempotent() {
        let app = mock_app();
        let first = open(&app.handle().clone()).expect("首次打开");
        assert_eq!(first.label(), LABEL);

        let second = open(&app.handle().clone()).expect("窗口仍在时重开不该失败");
        assert_eq!(second.label(), LABEL);

        // 标签唯一：重开拿到的是同一个窗口，不是第二个
        let labels: Vec<String> = app
            .webview_windows()
            .keys()
            .filter(|l| *l == LABEL)
            .cloned()
            .collect();
        assert_eq!(labels.len(), 1, "重开不该产生第二个控制窗口");
    }

    /// 窗口不存在时 `toggle` 必须把它建出来 —— 托盘点击是用户在窗口被
    /// 销毁（而非隐藏）后唯一的入口。若这里只做显隐不做兜底建窗，
    /// 用户就再也见不到界面。
    #[test]
    fn toggle_creates_window_when_absent() {
        let app = mock_app();
        assert!(app.get_webview_window(LABEL).is_none(), "初始不该有控制窗口");
        toggle(&app.handle().clone());
        assert!(
            app.get_webview_window(LABEL).is_some(),
            "托盘切换必须能从零把控制窗口建出来"
        );
    }

    /// §12：「控制窗口关闭 → 代理继续运行，托盘常驻」。
    ///
    /// 销毁与隐藏的区别不是风格问题：非 macOS 上最后一个窗口销毁会终止
    /// 事件循环，代理跟着死。这条断言的是判决本身 —— MockRuntime 的
    /// `run_iteration` 是空实现，`on_window_event` 的闭包在测试里不会被
    /// 触发，所以判决必须提到闭包外面才测得到。
    #[test]
    fn closing_hides_instead_of_destroying() {
        assert_eq!(
            close_action(),
            CloseAction::HideAndKeepRunning,
            "关闭控制窗口绝不能销毁它：窗口没了，代理跟着没"
        );
    }

    /// 用户关掉窗口（→ 隐藏，标签仍在）之后再从托盘打开，必须重新可见。
    ///
    /// 直接用 `hide()` 模拟关闭后的状态 —— 这正是 `close_action` 判决落到
    /// 窗口上的结果。随后的 `open` 必须走 show/focus 而不是报标签重复。
    #[test]
    fn a_hidden_window_can_be_reopened() {
        let app = mock_app();
        let w = open(&app.handle().clone()).expect("首次打开");
        w.hide().expect("隐藏（等同于用户点关闭）");

        // 隐藏不销毁：标签仍在注册表里，这正是重开必须早退的原因
        assert!(
            app.get_webview_window(LABEL).is_some(),
            "隐藏之后窗口必须还在 —— 若这里没了，说明走的是销毁路径"
        );

        let again = open(&app.handle().clone()).expect("关闭后重开不该失败");
        assert_eq!(again.label(), LABEL);
        assert_eq!(
            app.webview_windows().keys().filter(|l| *l == LABEL).count(),
            1,
            "重开不该产生第二个控制窗口"
        );
    }

    /// 控制窗口的标签是 capability 隔离的锚点：`capabilities/control.json`
    /// 的 `windows` 写死 "control"，两者一旦对不上，控制页的每个 invoke
    /// 都会被 ACL 拒掉（而 ACL 拒绝在 UI 上表现为「命令不存在」，极难定位）。
    #[test]
    fn label_matches_the_control_capability() {
        let cap: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("capabilities/control.json"),
            )
            .expect("读取 control.json"),
        )
        .expect("解析 control.json");
        let windows = cap["windows"].as_array().expect("control.json 缺 windows");
        assert!(
            windows.iter().any(|w| w == LABEL),
            "control.json 的 windows 里没有 {LABEL}"
        );
    }
}

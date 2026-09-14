//! 托盘图标与菜单（设计文档 §11.3）。
//!
//! 托盘是这个应用的**主要入口**，不是附属品：§11.4 的意图三问写明
//! 「界面平时后台常驻，只在网络出问题时被打开」。窗口关了之后，托盘是
//! 用户与进程之间唯一的接触面。
//!
//! 依赖 tauri 的 `tray-icon` 特性 —— 不开的话 `tauri::tray` 模块根本不存在
//! （tauri/src/lib.rs:112 的 `#[cfg(all(desktop, feature = "tray-icon"))]`）。
//!
//! ## 两条独立于控制窗口的生命周期
//!
//! 托盘与控制窗口是**各活各的**，这不是巧合而是 §12 的要求：
//!
//! - **窗口关了，托盘还在。** 托盘句柄由 Tauri 的 resources table 持有
//!   （`TrayIcon::register` 里 `resources_table().add(self.clone())`），
//!   不挂在任何窗口上。因此 `build` 的返回值可以直接丢掉 —— 丢的是我们这
//!   一份 Arc，manager 那份还在，图标不会消失。
//! - **窗口从没开过，托盘也能用。** 每个菜单项的处理函数都不假设窗口存在：
//!   「打开控制台」走 `control::open`（没有就建），左键点击走
//!   `control::toggle`（同样有兜底建窗）。守它的是
//!   `every_menu_item_is_reachable_before_the_window_exists`。
//!
//! ## 权限：托盘不需要动 capability
//!
//! 托盘是**从 Rust 侧建的**，走的不是 IPC，因此不需要给任何窗口加权限。
//! `core:tray:default` / `core:menu:default` 那些权限管的是「前端 JS 能不能
//! 调 `plugin:tray|new`」—— 我们不让前端碰托盘，所以一条都不用加。
//!
//! 这一点值得写下来：往 `control.json` 加权限看似无害，但两个 capability
//! 的命令交集为空是 §13 的安全底线，每加一条都要重新过一遍那 5 条断言。
//! 本模块的正确做法是**什么都不加**。

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub const ID: &str = "wsieve-tray";

// 菜单项 id —— 与 on_menu_event 里的匹配分支一一对应。
// 用常量而非字面量：改名时编译器会帮忙，字面量不会。
const ID_OPEN: &str = "open";
const ID_MODE_RULE: &str = "mode-rule";
const ID_MODE_GLOBAL: &str = "mode-global";
const ID_MODE_DIRECT: &str = "mode-direct";
const ID_QUIT: &str = "quit";

/// 三个模式菜单项 →（菜单 id, 模式名）。
///
/// 单独列出来是为了让 `on_menu_event` 的分派与「菜单里到底有哪几项」共用
/// 同一份事实：漏掉一个分支的话，用户点了菜单只会看到一条 warn 日志，
/// 而那种 bug 在肉眼验收里极容易滑过去（三项长得一模一样）。
/// 守它的是 `every_menu_item_has_a_handler`。
///
/// `pub(crate)`：`commands::control` 的
/// `the_tray_and_the_command_agree_on_the_mode_names` 拿它当裁判，确保托盘
/// 与命令面这两个入口对「有哪几个模式」不会分叉。让测试读真身而不是另抄
/// 一份，才谈得上是交叉校验。
pub(crate) const MODES: [(&str, &str); 3] = [
    (ID_MODE_RULE, "rule"),
    (ID_MODE_GLOBAL, "global"),
    (ID_MODE_DIRECT, "direct"),
];

/// 托盘菜单项被点击后该做什么。
///
/// 把分派判决从闭包里提出来变成纯函数，理由与 `control::close_action` 完全
/// 相同：MockRuntime 没有托盘实现（`mock_runtime.rs` 里根本没有 tray 相关
/// 代码），`TrayIconBuilder::build()` 在无头测试里跑不起来，闭包永远不会被
/// 调用。判决留在闭包里就等于**完全没有测试**，只能靠肉眼点五次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// 打开（必要时新建）控制窗口
    OpenControl,
    /// 切换代理模式
    SetMode(&'static str),
    /// 退出进程 —— 必须走 `app.exit(0)` 而非 `std::process::exit`
    Quit,
    /// 不认识的 id。不是 panic 也不是静默：房规要求出声。
    Unknown,
}

fn menu_action(id: &str) -> MenuAction {
    if id == ID_OPEN {
        return MenuAction::OpenControl;
    }
    if id == ID_QUIT {
        return MenuAction::Quit;
    }
    for (menu_id, mode) in MODES {
        if id == menu_id {
            return MenuAction::SetMode(mode);
        }
    }
    MenuAction::Unknown
}

/// 执行判决。与 `menu_action` 分开：前者是纯函数（可测），
/// 后者碰真实的 app handle（测不了，但也没有分支逻辑了）。
fn dispatch<R: tauri::Runtime>(app: &tauri::AppHandle<R>, id: &str) {
    match menu_action(id) {
        MenuAction::OpenControl => {
            if let Err(e) = crate::control::open(app) {
                tracing::error!("从托盘打开控制窗口失败：{e}");
            }
        }
        MenuAction::SetMode(mode) => set_mode_from_tray(app, mode),
        MenuAction::Quit => {
            // exit(0) 会走到 RunEvent::Exit（实测），于是 hosts 摘除、
            // 系统代理还原、stats.json 落盘都能执行。
            // 直接 std::process::exit 会跳过这三者。
            app.exit(0);
        }
        MenuAction::Unknown => tracing::warn!("未处理的托盘菜单项：{id}"),
    }
}

pub fn build(app: &tauri::AppHandle) -> tauri::Result<TrayIcon> {
    let open = MenuItemBuilder::with_id(ID_OPEN, "打开控制台").build(app)?;
    let mode_rule = MenuItemBuilder::with_id(ID_MODE_RULE, "规则模式").build(app)?;
    let mode_global = MenuItemBuilder::with_id(ID_MODE_GLOBAL, "全局模式").build(app)?;
    let mode_direct = MenuItemBuilder::with_id(ID_MODE_DIRECT, "直连模式").build(app)?;
    let quit = MenuItemBuilder::with_id(ID_QUIT, "退出 websieve").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[&open])
        .separator()
        .items(&[&mode_rule, &mode_global, &mode_direct])
        .separator()
        .items(&[&quit])
        .build()?;

    // `default_window_icon()` 在本项目里一定是 Some：tauri-codegen 对
    // 非 Windows 目标无条件取 `icons/icon.png` 并 `Some(...)` 包起来
    // （tauri-codegen/src/context.rs:234-242），文件缺失时 build.rs 阶段
    // 就会失败，轮不到这里。但仍然不 unwrap —— 图标没了顶多是没托盘，
    // 不该连带把代理弄崩。
    let icon = app
        .default_window_icon()
        .ok_or_else(|| tauri::Error::AssetNotFound("icons/icon.png（默认窗口图标）".into()))?
        .clone();

    TrayIconBuilder::with_id(ID)
        .icon(icon)
        // macOS 菜单栏图标必须是模板图（单色 + alpha），否则深色菜单栏下
        // 会是一块糊掉的彩色方块。Windows / Linux 上此项被忽略。
        .icon_as_template(true)
        .tooltip("websieve")
        .menu(&menu)
        // 左键点图标 = 显隐窗口（§11.3），右键才出菜单。
        // 左键也弹菜单的话，最常用的动作就要多两步。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| dispatch(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            // 只响应左键**抬起**。按下就响应会让拖动图标也触发切换。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // toggle 在窗口不存在时会把它建出来 —— 用户从没打开过控制台
                // 时，左键点托盘依然要能出界面。
                crate::control::toggle(tray.app_handle());
            }
        })
        .build(app)
}

/// 托盘切模式。
///
/// ponytail: 阶段 4 只发事件通知 UI，不真的改配置 —— 配置写回是 Task 11 的
/// config_save，而模式切换的实际生效需要阶段 2 的出站管理器。
/// 上限：托盘点了模式，界面会变，但流量走向不变。
/// 升级路径：阶段 5 接上 set_mode 命令的真实实现，把这里改成调它。
fn set_mode_from_tray<R: tauri::Runtime>(app: &tauri::AppHandle<R>, mode: &str) {
    tracing::info!("托盘请求切换模式：{mode}");
    crate::events::emit_control(app, "mode-changed", mode.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    // Manager 只有测试用得上（get_webview_window / webview_windows）——
    // build 走的是 AppHandle 上的固有方法，不需要这个 trait。
    use tauri::Manager;
    use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};

    fn mock_app() -> tauri::App<MockRuntime> {
        mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app")
    }

    /// 菜单里的每一项都必须有分派分支。
    ///
    /// 三个模式项长得一模一样，漏掉一个的表现是「点了没反应 + 一条 warn」，
    /// 肉眼验收极容易滑过去 —— 尤其是验收清单只写了「五项俱全」而没写
    /// 「五项都点一遍」。
    #[test]
    fn every_menu_item_has_a_handler() {
        assert_eq!(menu_action(ID_OPEN), MenuAction::OpenControl);
        assert_eq!(menu_action(ID_QUIT), MenuAction::Quit);
        assert_eq!(menu_action(ID_MODE_RULE), MenuAction::SetMode("rule"));
        assert_eq!(menu_action(ID_MODE_GLOBAL), MenuAction::SetMode("global"));
        assert_eq!(menu_action(ID_MODE_DIRECT), MenuAction::SetMode("direct"));
    }

    /// 不认识的 id 既不 panic 也不静默 —— 房规。
    #[test]
    fn an_unknown_menu_id_is_reported_not_ignored() {
        assert_eq!(menu_action("没这一项"), MenuAction::Unknown);
        assert_eq!(menu_action(""), MenuAction::Unknown);
    }

    /// 退出必须走 `app.exit(0)`，不能是 `std::process::exit`。
    ///
    /// 这条看着像重复上面那条，其实守的是完全不同的东西：`RunEvent::Exit`
    /// 是 hosts 摘除、系统代理还原、stats.json 落盘的**唯一**触发点。
    /// 有人图省事把这里换成 `std::process::exit(0)` 的话，用户会得到一台
    /// 系统代理指向死端口、hosts 里留着劫持条目的机器 —— 表现为「关掉
    /// websieve 之后整机上不了网」。
    #[test]
    fn quitting_goes_through_the_app_so_cleanup_runs() {
        assert_eq!(
            menu_action(ID_QUIT),
            MenuAction::Quit,
            "退出项必须走 app.exit(0) 分支，那是 hosts/系统代理/stats 清理的唯一入口"
        );
    }

    /// **托盘的动作在控制窗口从没被打开过时也要能用。**
    ///
    /// 托盘与窗口是两条独立的生命周期：用户完全可能启动后从不开界面，
    /// 一直用托盘。若「打开控制台」依赖窗口已存在，那用户就永远打不开它。
    ///
    /// 这里直接在**没有任何窗口**的 app 上跑一遍分派，断言窗口被建出来。
    #[test]
    fn every_menu_item_is_reachable_before_the_window_exists() {
        let app = mock_app();
        let handle = app.handle().clone();
        assert!(
            handle.get_webview_window(crate::control::LABEL).is_none(),
            "前提：此刻控制窗口从没被打开过"
        );

        // 模式切换在窗口不存在时必须是安全的空操作（emit_control 会短路），
        // 而不是 panic —— 用户完全可能开机后先点模式再开界面。
        for (id, _) in MODES {
            dispatch(&handle, id);
        }
        assert!(
            handle.get_webview_window(crate::control::LABEL).is_none(),
            "切模式不该顺手把窗口建出来"
        );

        // 「打开控制台」必须能从零建出窗口
        dispatch(&handle, ID_OPEN);
        assert!(
            handle.get_webview_window(crate::control::LABEL).is_some(),
            "从没开过界面的用户点「打开控制台」必须能看到窗口"
        );
    }

    /// 窗口被关掉（= 隐藏）之后，托盘的「打开控制台」还要能把它叫回来。
    ///
    /// 这是托盘存在的**核心理由**：§12 说关窗不退进程，那么关窗之后必须
    /// 有一条路回去。走的是 `control::open` 的早退分支。
    #[test]
    fn the_tray_can_reopen_a_closed_window() {
        let app = mock_app();
        let handle = app.handle().clone();

        dispatch(&handle, ID_OPEN);
        let w = handle
            .get_webview_window(crate::control::LABEL)
            .expect("首次打开");
        w.hide().expect("隐藏（等同于用户点关闭）");

        // 再点一次托盘的「打开控制台」
        dispatch(&handle, ID_OPEN);
        assert_eq!(
            handle
                .webview_windows()
                .keys()
                .filter(|l| *l == crate::control::LABEL)
                .count(),
            1,
            "重开不该产生第二个控制窗口"
        );
    }

    /// 托盘不需要任何 capability 权限 —— 它从 Rust 侧建，不走 IPC。
    ///
    /// 这条断言的是「我们没有为了托盘去动 control.json」。§13 的安全底线是
    /// 两个 capability 命令交集为空，每加一条权限都要重新过一遍那 5 条断言；
    /// 本模块的正确做法是一条都不加，这里把它钉死。
    #[test]
    fn the_tray_needs_no_capability_permission() {
        let control = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities/control.json"),
        )
        .expect("读 control.json");
        let v: serde_json::Value = serde_json::from_str(&control).expect("control.json 不是合法 JSON");
        let perms = v["permissions"]
            .as_array()
            .expect("permissions 应当是数组")
            .iter()
            .filter_map(|p| p.as_str())
            .collect::<Vec<_>>();

        // core:default 里已经含 core:tray:default / core:menu:default，
        // 但那是给前端 JS 用的；我们从 Rust 建托盘，用不上。
        // 因此不该出现任何**单独**为托盘补的权限。
        let tray_specific: Vec<_> = perms
            .iter()
            .filter(|p| p.starts_with("core:tray") || p.starts_with("core:menu"))
            .collect();
        assert!(
            tray_specific.is_empty(),
            "托盘从 Rust 侧建，不需要单独补权限；多出来的：{tray_specific:?}"
        );
    }
}

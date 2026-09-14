//! 连接控制命令（设计文档 §11.2）。
//!
//! 本阶段的定位：**接口定型**。`connect` / `disconnect` / `set_mode` /
//! `outbound_enable` 的真实实现依赖阶段 2 的多出站管理器 —— 而当前的
//! `OutboundManager` 是在 `main.rs` 的 `run_stack` 里自建、自看护的，
//! 没有暴露给命令面的外部开关。
//!
//! 这里返回 `CmdError::NotReady` 而不是假成功：房规禁止 mock。
//! 「还没接上」是当前的事实，UI 照实显示即可；假装连上了才是伪实现，
//! 而且是最坏的那一种 —— 用户以为流量在走代理，实际在裸奔。
//!
//! `control_hide` 与 `app_quit` 例外：这两条**在本阶段就是完整实现**，
//! 不依赖任何后续阶段。

use super::{CmdError, CmdResult};
use crate::events;

/// 合法的分流模式（设计文档 §5.2 的 `mode` 字段）。
///
/// 判定单独提成纯函数而非埋在命令体里，是为了能穷举测 —— 命令本身要
/// `AppHandle`，而这条判定的正确性与 Tauri 毫无关系。这也是本仓库既有的
/// 风格（阶段 1 的路由引擎就是纯函数 + 穷举单测）。
pub const MODES: [&str; 3] = ["rule", "global", "direct"];

/// 是否是一个合法模式。
///
/// **刻意不 trim、不忽略大小写**，尽管下游 `wsieve_route::Mode::from_str`
/// 两者都做。理由是来源不同：`from_str` 处理的是**用户手写的 YAML**，
/// 宽容是对的；这里处理的是**UI 传来的枚举值**，那是我们自己的代码生成的，
/// 出现 `"RULE"` 或 `" rule "` 只可能是前端出了 bug。悄悄纠正它等于把这个
/// bug 藏起来，下次它以别的形式冒出来时没人知道根在哪。
pub fn valid_mode(m: &str) -> bool {
    MODES.contains(&m)
}

/// 建立全部启用的出站连接。
#[tauri::command]
pub async fn connect(app: tauri::AppHandle) -> CmdResult<()> {
    // 事件照推：UI 需要知道「请求收到了但没生效」，而不是点了按钮毫无反应。
    // 注意这条事件的文案不能暗示已连上 —— 那就成了假成功。
    events::emit_status(&app, "connect 请求已收到，但出站管理器尚未暴露外部开关");
    Err(CmdError::not_ready(
        "connect",
        "出站管理器目前在 run_stack 里自启动自重连，没有外部 start/stop",
    ))
}

#[tauri::command]
pub async fn disconnect(app: tauri::AppHandle) -> CmdResult<()> {
    events::emit_status(&app, "disconnect 请求已收到，但出站管理器尚未暴露外部开关");
    Err(CmdError::not_ready(
        "disconnect",
        "出站管理器目前在 run_stack 里自启动自重连，没有外部 start/stop",
    ))
}

/// 切换分流模式。
///
/// 校验先于一切：不认识的值当场拒绝，且**在推任何事件之前**。先推事件再
/// 报错会让 UI 与托盘按一个从未生效的模式更新选中态，界面与实际从此不一致。
#[tauri::command]
pub async fn set_mode(app: tauri::AppHandle, mode: String) -> CmdResult<()> {
    if !valid_mode(&mode) {
        return Err(CmdError::ConfigInvalid {
            message: format!("未知模式 {mode:?}，只接受 rule / global / direct"),
        });
    }
    // 模式合法，但落盘与生效还没接上。事件仍然要推 —— 托盘的模式菜单
    // （tray.rs 的 set_mode_from_tray）走的是同一条事件，两个入口的行为
    // 必须一致，否则「从托盘切」和「从界面切」会给出不同的界面反馈。
    events::emit_control(&app, "mode-changed", mode.clone());
    Err(CmdError::not_ready(
        "set_mode 的落盘与生效",
        "需要把 RuleSet 换成可热替换的，见阶段 5",
    ))
}

#[tauri::command]
pub async fn outbound_enable(id: String, enabled: bool) -> CmdResult<()> {
    // 参数照样先校验：空出站名是前端 bug，不该等到有实现了才被发现。
    if id.trim().is_empty() {
        return Err(CmdError::ConfigInvalid {
            message: "出站名不能为空".into(),
        });
    }
    let _ = enabled;
    Err(CmdError::not_ready(
        "outbound_enable",
        "出站的启停开关在阶段 2 的出站管理器里，尚未接到命令面",
    ))
}

/// 隐藏控制窗口（UI 里的「最小化到托盘」）。
///
/// 这一条在本阶段就是完整实现。隐藏而非关闭是 §12 的要求：代理继续跑，
/// 托盘常驻，窗口标签不释放（`control::open` 因此有那个早退分支）。
#[tauri::command]
pub async fn control_hide(app: tauri::AppHandle) -> CmdResult<()> {
    use tauri::Manager;
    let w = app
        .get_webview_window(crate::control::LABEL)
        .ok_or_else(|| CmdError::other("控制窗口不存在"))?;
    w.hide().map_err(CmdError::other)
}

/// 退出应用。
///
/// 走 `app.exit(0)` 而不是 `std::process::exit` —— 后者会跳过
/// `RunEvent::Exit`，而 hosts 摘除、系统代理恢复、命中计数落盘全挂在那里。
/// 跳过它的后果是用户整机断网加域名指向一个已经不在跑的转发器。
#[tauri::command]
pub async fn app_quit(app: tauri::AppHandle) -> CmdResult<()> {
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_three_documented_modes_are_accepted() {
        for good in MODES {
            assert!(valid_mode(good), "{good} 是 §5.2 列出的合法模式");
        }
        for bad in ["", "auto", "script", "global-direct", "规则"] {
            assert!(!valid_mode(bad), "{bad:?} 不该被当作合法模式");
        }
    }

    #[test]
    fn near_misses_from_the_ui_are_rejected_rather_than_normalized() {
        // 这些值下游的 Mode::from_str 全都收得下（它 trim + 转小写）。这里
        // 刻意更严：来源是我们自己的前端代码，出现这些值只可能是 bug，
        // 悄悄纠正等于把 bug 藏起来。若哪天决定改成宽松，这条会提醒你
        // 那是一次有意的语义变更，而不是顺手放宽。
        for near in ["RULE", "Rule", " rule ", "rule ", "\tglobal\n", "DIRECT"] {
            assert!(
                !valid_mode(near),
                "{near:?} 与合法值只差大小写/空白，但仍应被拒 —— UI 不该传出这种值"
            );
        }
    }

    #[test]
    fn the_mode_list_matches_what_the_routing_engine_understands() {
        // 两处各写一份合法值清单，迟早会分叉：这里加了一个模式而引擎不认，
        // 用户就会看到「切换成功」然后什么也没变。用引擎自己当裁判。
        use std::str::FromStr;
        for m in MODES {
            wsieve_route::Mode::from_str(m)
                .unwrap_or_else(|e| panic!("{m} 应被路由引擎接受：{e}"));
        }
    }

    #[test]
    fn the_tray_and_the_command_agree_on_the_mode_names() {
        // 托盘菜单（tray.rs 的 MODES）与本命令是同一个功能的两个入口，各自
        // 维护一份模式名清单的话，「从托盘切」和「从界面切」会走向不同的
        // 分支 —— 而那种不一致在肉眼验收里几乎看不出来。
        let mut from_tray: Vec<&str> = crate::tray::MODES.iter().map(|(_, m)| *m).collect();
        from_tray.sort_unstable();
        let mut mine = MODES.to_vec();
        mine.sort_unstable();
        assert_eq!(from_tray, mine, "托盘与命令面的模式清单必须一致");

        // 反向：托盘推出去的每一个模式名都得过本命令的校验，否则用户点了
        // 托盘菜单，命令面却认为那是个非法值。
        for (_, m) in crate::tray::MODES {
            assert!(valid_mode(m), "托盘的 {m:?} 过不了 set_mode 的校验");
        }
    }
}

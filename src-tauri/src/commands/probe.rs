//! 探针与观测命令（设计文档 §11.2 / §11.5）。

use serde::Serialize;

use super::{CmdError, CmdResult};

/// `rule_test` 的返回。字段对应设计文档 §11.2 的
/// `{ index, decision, tried, resolved }`。
#[derive(Debug, Serialize)]
pub struct RuleTestResult {
    /// 命中的规则下标（0-based）。MATCH 兜底时是它自己的下标
    pub index: usize,
    /// "DIRECT" | "REJECT" | 出站名
    pub decision: String,
    /// 命中之前试过多少条 —— §11.5 的「前 N 条已试未命中」就用它
    pub tried: usize,
    /// 是否触发了 DNS 解析；None 表示第一轮就判完（零解析）
    pub resolved: Option<Vec<String>>,
}

/// 规则试算探针（§11.5 signature ②）。
///
/// `resolve` 直接对应设计文档 §4.2 的两阶段求值：
///   false → 只跑第一轮（快、不发 DNS）
///   true  → 遇 `NeedResolve` 时解析后跑第二轮（准）
///
/// ponytail: 本阶段命令面拿不到正在服役的 `RuleSet` —— 它在 `run_stack` 的
/// 局部作用域里，没有进 managed state。
/// 上限：规则视图的试算探针在此之前不可用，命令如实报未就绪。
/// 升级路径：把 `Arc<Router>`（而非 `Arc<RuleSet>`）manage 进去，这里改成
/// 调 `router.decide(&target)`。**必须复用 `decide()` 而不是另写一遍两阶段
/// 循环** —— §4.2 纪律①要求试算结果与真实判决永远一致，而
/// `router.rs` 的模块注释已经记了一次教训：阶段 2 曾自带一份同样的循环，
/// 阶段 3 接入 `wsieve_dns::decide()` 后立刻删掉，理由正是「两份循环迟早
/// 有一份先被改动而另一份不知道」。试算与实际不一致的排查工具比没有更糟。
#[tauri::command]
pub async fn rule_test(target: String, resolve: bool) -> CmdResult<RuleTestResult> {
    // 参数校验不等实现 —— 空目标是前端 bug，现在就该说出来。
    if target.trim().is_empty() {
        return Err(CmdError::ConfigInvalid {
            message: "试算目标不能为空".into(),
        });
    }
    let _ = resolve;
    Err(CmdError::not_ready(
        "rule_test",
        "正在服役的 RuleSet 尚未进 managed state，见阶段 5",
    ))
}

/// 出站延迟探测（§11.2 的延迟指标定义）。
///
/// 语义：发一个 PADDING TU 的 POST 并计时，即触发一次稳态测量。
/// 不另开探测子流、不引入服务端探测端点 —— 复用既有流量路径，既省实现
/// 也少一个可探测面。
///
/// ponytail: 依赖阶段 2 的出站管理器暴露句柄，同 `connect`。
/// 上限：UI 的延迟列显示「未知」。
/// 升级路径：manage 进 `OutboundManager` 后，这里改成
/// `mgr.get(&id).ok_or(...)?.probe_latency().await`。
#[tauri::command]
pub async fn outbound_latency_probe(id: String) -> CmdResult<u64> {
    if id.trim().is_empty() {
        return Err(CmdError::ConfigInvalid {
            message: "出站名不能为空".into(),
        });
    }
    Err(CmdError::not_ready(
        "outbound_latency_probe",
        "出站管理器尚未接到命令面",
    ))
}

/// 拉取 GEO 数据。
///
/// ponytail: 下载、校验、原子替换、失败回滚是一整套，本阶段不引入。
/// 上限：用户得自己把 geoip.dat / geosite.dat 放到位（`geo_status` 告诉他
/// 放没放到位，见下）。
/// 升级路径：按 `geox-url` 配置下载到临时文件、校验后 rename，并一并写
/// `geo.meta.json` 记下时间戳（`geo_status.updated_at` 就有值了）。
#[tauri::command]
pub async fn geo_update() -> CmdResult<()> {
    Err(CmdError::not_ready(
        "geo_update",
        "GEO 数据的下载与原子替换尚未实现，可先手工放置数据文件",
    ))
}

#[derive(Debug, Serialize)]
pub struct GeoStatus {
    pub geoip_present: bool,
    pub geosite_present: bool,
    /// 查的是哪两个路径。**必须回报**：这两个文件的位置由环境变量决定
    /// （见 `crate::geo_path`），UI 只说「没找到」而不说去哪找过，用户
    /// 无从判断是自己放错了地方还是程序看错了地方。
    pub geoip_path: String,
    pub geosite_path: String,
    /// 上次更新时间。
    ///
    /// ponytail: 需要一份元数据文件才能知道，而 `geo_update` 尚未实现，
    /// 没有任何一条路径会写它。**这里返回 None 而不是拿文件的 mtime 冒充**
    /// —— 用户手工拷贝进来的文件，mtime 是拷贝时刻，与「这份数据有多新」
    /// 毫无关系。给一个看着像真的假时间戳，比诚实地说不知道更糟。
    /// 升级路径：`geo_update` 落盘时一并写 `geo.meta.json`。
    pub updated_at: Option<String>,
}

/// GEO 数据是否就位。
///
/// 这一条**在本阶段就是完整实现**：它回答的是「文件在不在」，而那与后续
/// 阶段无关。查的路径与 `main.rs` 真正加载 GEO 时用的是同一个
/// `crate::geo_path` —— 各查各的话，UI 会对着一个程序根本不读的路径
/// 报告「已就位」。
#[tauri::command]
pub async fn geo_status() -> CmdResult<GeoStatus> {
    let geoip = crate::geo_path("WSIEVE_GEOIP", "geoip.dat");
    let geosite = crate::geo_path("WSIEVE_GEOSITE", "geosite.dat");
    Ok(GeoStatus {
        geoip_present: geoip.is_file(),
        geosite_present: geosite.is_file(),
        geoip_path: geoip.display().to_string(),
        geosite_path: geosite.display().to_string(),
        updated_at: None,
    })
}

/// 流量快照 —— 供控制窗口刚打开时补齐历史，不必等下一个 1s tick。
///
/// 这一条**在本阶段就是完整实现**：数据源是已经 manage 进去的聚合器。
#[tauri::command]
pub async fn traffic_snapshot(
    agg: tauri::State<'_, crate::events::Aggregator>,
) -> CmdResult<crate::events::TrafficSample> {
    use std::sync::atomic::Ordering;
    let c = &agg.counters;
    Ok(crate::events::TrafficSample {
        up_bytes: c.up_total.load(Ordering::Relaxed),
        down_bytes: c.down_total.load(Ordering::Relaxed),
        // 快照不含速率 —— 速率是「相对上一次采样」的概念，而快照没有
        // 上一次。编一个数字填进去就是伪数据；UI 等下一个 traffic 事件即可。
        up_rate: 0,
        down_rate: 0,
        active: c.active.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未就绪的命令必须**报错**，不能返回一个看着像真的零值。
    ///
    /// 这条守的是房规里最要紧的一条：`Ok(0)` 会让 UI 显示「延迟 0ms」，
    /// 用户据此认为这个出站是最快的 —— 一个编造的数字比一句「还没接上」
    /// 危险得多。
    #[tokio::test]
    async fn unimplemented_probes_report_not_ready_instead_of_a_plausible_zero() {
        let e = outbound_latency_probe("日本节点".into()).await.unwrap_err();
        assert!(matches!(e, CmdError::NotReady { .. }), "{e:?}");

        let e = rule_test("example.com:443".into(), false).await.unwrap_err();
        assert!(matches!(e, CmdError::NotReady { .. }), "{e:?}");

        let e = geo_update().await.unwrap_err();
        assert!(matches!(e, CmdError::NotReady { .. }), "{e:?}");
    }

    /// 未就绪的错误必须说清在等什么。
    #[tokio::test]
    async fn not_ready_errors_name_what_they_are_waiting_for() {
        let e = rule_test("example.com:443".into(), false).await.unwrap_err();
        let text = e.to_string();
        assert!(text.contains("rule_test"), "要点名是哪个功能：{text}");
        assert!(
            text.contains("RuleSet") || text.contains("阶段"),
            "要说清在等什么：{text}"
        );
    }

    /// 参数校验不该等实现 —— 空参数是前端 bug，现在就能说出来。
    #[tokio::test]
    async fn bad_arguments_are_rejected_before_the_not_ready_path() {
        for bad in ["", "   "] {
            let e = rule_test(bad.into(), false).await.unwrap_err();
            assert!(
                matches!(e, CmdError::ConfigInvalid { .. }),
                "空目标该报参数错而非未就绪：{e:?}"
            );
            let e = outbound_latency_probe(bad.into()).await.unwrap_err();
            assert!(matches!(e, CmdError::ConfigInvalid { .. }), "{e:?}");
        }
    }

    /// GEO 状态查的必须是程序真正加载的那两个路径。
    ///
    /// 各查各的话，UI 会对着一个程序根本不读的位置报告「已就位」，
    /// 而代理那边的 GEO 规则一条都不命中 —— 这类不一致排查起来极其费劲。
    #[tokio::test]
    async fn geo_status_looks_where_the_loader_actually_looks() {
        let s = geo_status().await.unwrap();
        let expect_ip = crate::geo_path("WSIEVE_GEOIP", "geoip.dat");
        let expect_site = crate::geo_path("WSIEVE_GEOSITE", "geosite.dat");
        assert_eq!(s.geoip_path, expect_ip.display().to_string());
        assert_eq!(s.geosite_path, expect_site.display().to_string());
        // 存在性判定与路径同源
        assert_eq!(s.geoip_present, expect_ip.is_file());
        assert_eq!(s.geosite_present, expect_site.is_file());
    }

    /// 时间戳没有出处时必须是 None。
    ///
    /// 拿文件 mtime 冒充「数据有多新」是本模块最容易犯的错：用户手工拷贝
    /// 进来的文件，mtime 是拷贝时刻，可能是一份三年前的数据。
    #[tokio::test]
    async fn geo_status_does_not_fabricate_a_timestamp() {
        assert!(
            geo_status().await.unwrap().updated_at.is_none(),
            "没有元数据文件时不能编一个时间戳出来"
        );
    }

    /// 快照的速率位是 0，且这是**有意的**语义而非漏填。
    #[test]
    fn a_snapshot_carries_totals_but_no_rate() {
        use std::sync::atomic::Ordering;
        let agg = crate::events::Aggregator::new();
        agg.counters.up_total.store(4096, Ordering::Relaxed);
        agg.counters.down_total.store(8192, Ordering::Relaxed);
        agg.counters.active.store(3, Ordering::Relaxed);

        // 直接构造与命令体一致的快照（命令要 State，而 State 只能由
        // 运行中的 app 给出；这里验的是「读的是哪几个计数器」）。
        let c = &agg.counters;
        let s = crate::events::TrafficSample {
            up_bytes: c.up_total.load(Ordering::Relaxed),
            down_bytes: c.down_total.load(Ordering::Relaxed),
            up_rate: 0,
            down_rate: 0,
            active: c.active.load(Ordering::Relaxed),
        };
        assert_eq!(s.up_bytes, 4096);
        assert_eq!(s.down_bytes, 8192);
        assert_eq!(s.active, 3);
        assert_eq!(s.up_rate, 0, "快照没有「上一次」，速率只能是 0");
    }
}

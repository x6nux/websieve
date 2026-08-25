// IPC 的唯一入口。集中在这里是为了让「控制窗口调了哪些命令」可被 grep ——
// 该清单必须与 capabilities/control.json 逐条对应，多一个就是权限泄漏。
//
// withGlobalTauri 为 true（tauri.conf.json），因此走 window.__TAURI__ 而不必
// 引 @tauri-apps/api 包。少一个 npm 依赖，且版本永远与 Rust 侧一致。

export function invoke(cmd, args) {
  return window.__TAURI__.core.invoke(cmd, args);
}

export function listen(event, handler) {
  return window.__TAURI__.event.listen(event, handler);
}

// ── 配置（设计文档 §11.2）────────────────────────────────────────

// 结构化读。返回 { config, rules }：
//   config —— 全量配置的 JSON 投影，**client-priv 已脱敏为 "***"**，
//             且不含 rules 键（规则只有 rules 一个出处，见下）
//   rules  —— [{ value, line }]，line 是 config.yaml 里的 1-based 行号，
//             configSave 的定点改写全靠它，别在前端把它丢掉
export function configGet() {
  return invoke('config_get');
}

// 原文读 —— **返回值含明文 client-priv 私钥**。
//
// 调用点必须满足两条，否则不要用它：
//   1. 是用户显式要求看原文/导出（§5.4 要求在该入口给出「此文件含私钥」的警告）
//   2. 返回值不进 console、不进任何错误上报、不塞进会被截图的 DOM
//
// 看规则、切模式、看流量都不需要私钥 —— 那些一律走 configGet。
export function configGetRaw() {
  return invoke('config_get_raw');
}

// 结构化写：按行号定点改写规则，用户手写的注释一个字都不会丢。
//
// ops 是一个数组，每项形如
//   { op: 'replace-rule', line, expect, value }
//   { op: 'delete-rule',  line, expect }
// expect 是「你看到的那一行现在是什么」。文件在读与写之间被改过时，服务端
// 靠它发现行号已陈旧并**拒绝**本次保存 —— 没有它就会照旧行号改到别的规则头上。
// 多条操作的施加顺序由 Rust 侧负责，前端不必自己从下往上排。
export function configSave(ops) {
  return invoke('config_save', { ops });
}

// 原文写 —— 逃生舱（§5.6），一字不动地覆盖。
// 写入前 Rust 侧会解析并校验，语法或语义不合法会带着行列号被拒。
export function configSaveRaw(text) {
  return invoke('config_save_raw', { text });
}

// ── 连接控制（设计文档 §11.2）────────────────────────────────────
//
// connect / disconnect / setMode / outboundEnable 目前会以
// { kind: 'not-ready', message } 被拒 —— 出站管理器还没暴露外部开关。
// 这是**真实响应**而不是占位：照实显示这条 message 即可，别把它当成
// 「成功了但没反馈」来处理，更别在前端伪造一个成功态。

export function connect() {
  return invoke('connect');
}

export function disconnect() {
  return invoke('disconnect');
}

// mode 只接受 'rule' | 'global' | 'direct'，**不做 trim、不忽略大小写**。
// 传 'RULE' 或 ' rule ' 会被拒 —— 那只可能是前端 bug，Rust 侧刻意不替我们
// 纠正（纠正等于把 bug 藏起来）。
export function setMode(mode) {
  return invoke('set_mode', { mode });
}

export function outboundEnable(id, enabled) {
  return invoke('outbound_enable', { id, enabled });
}

// 最小化到托盘。代理继续跑（§12），不是退出。**已完整实现。**
export function controlHide() {
  return invoke('control_hide');
}

// 退出应用。**已完整实现**，且走 app.exit(0) 以确保 hosts 摘除、
// 系统代理恢复、命中计数落盘都能跑到。
export function appQuit() {
  return invoke('app_quit');
}

// ── 探针与观测（设计文档 §11.2 / §11.5）─────────────────────────

// 规则试算。resolve=false 只跑第一轮（快、不发 DNS），true 则在遇到
// IP 类规则时解析后再跑第二轮（准）。UI 默认传 false。
// 目前以 not-ready 被拒 —— 服役中的规则集还没进 managed state。
export function ruleTest(target, resolve = false) {
  return invoke('rule_test', { target, resolve });
}

// 单个出站的延迟探测（毫秒）。目前以 not-ready 被拒。
// 注意它**不会**返回 0 表示「不知道」—— 那会被显示成「延迟 0ms，最快」。
export function outboundLatencyProbe(id) {
  return invoke('outbound_latency_probe', { id });
}

// 触发 GEO 数据更新。目前以 not-ready 被拒（可先手工放置数据文件）。
export function geoUpdate() {
  return invoke('geo_update');
}

// GEO 数据是否就位。**已完整实现。** 返回
//   { geoip_present, geosite_present, geoip_path, geosite_path, updated_at }
// 字段名是 snake_case（Rust 侧没开 rename_all，别按 camelCase 去取）。
// 两个路径要显示出来：文件位置由环境变量决定，只说「没找到」而不说去哪
// 找过，用户无从判断是自己放错了还是程序看错了。
// updated_at 恒为 null，直到 geoUpdate 落地并写下元数据 —— 别拿文件 mtime
// 在前端补一个，那与「数据有多新」毫无关系。
export function geoStatus() {
  return invoke('geo_status');
}

// 流量计数快照。**已完整实现。** 控制窗口刚打开时用它补齐历史，
// 不必空等下一个 1s 的 traffic 事件。
// 快照里的 up_rate / down_rate 恒为 0：速率是「相对上一次采样」的概念，
// 而快照没有上一次。速率等下一个 traffic 事件。
export function trafficSnapshot() {
  return invoke('traffic_snapshot');
}

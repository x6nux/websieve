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


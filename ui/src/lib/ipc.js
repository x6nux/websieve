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

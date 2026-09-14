# Windows 与 Android（VPN 模式）平台支持 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `wsieve-app` 在 Windows 上功能完整（多会话条带、系统代理、TUN），并新增 Android 平台，用 `VpnService` 承接全局流量。

**Architecture:** 两个平台复用**同一套 Rust 核心**，只补平台后端：
- Windows 补三块 —— `RouteBackend` 的 Win 实现、`ManagedSystemState` 的注册表代理实现、`spawn_netstack` 的 wintun 分支。三个抽象点**都已存在**，是填空不是重构。
- Android 走**外部 fd 模型**：TUN 设备由 Kotlin 的 `VpnService` 建立，把 fd 交给 Rust，`tun_rs::AsyncDevice::from_fd()` 接管。Android **不需要 bypass 路由**（见全局约束）。

**Tech Stack:** Rust / Tauri 2.11 / wry 0.55 / tun-rs 2.8 / netstack-smoltcp / Kotlin (Android) / windows-sys

**参考实现:** [EasyTier](https://github.com/EasyTier/EasyTier) 的 `tauri-plugin-vpnservice`（Tauri 2 + Android VpnService + Rust TUN 的完整范例，本计划的 Kotlin 部分照它的形状走）

---

## 调研结论（这些是查证过的事实，不是假设）

写这份计划前逐条验证过，直接影响任务拆分：

### 1. Windows 的 WebView 代理**能用**，且不需要任何 feature flag

`wry-0.55.1/src/webview2/mod.rs:304-318`：WebView2 后端把
`--proxy-server=socks5://<host>:<port>` 塞进 additional browser arguments。
`ProxyConfig::Socks5` 分支是现成的。

⇒ **多会话条带在 Windows 上走代理路，不必退回 hosts 劫持。**
`src-tauri/src/webview_proxy.rs` 整个模块可以原样复用。

`tauri` 的 `macos-proxy` feature 只管 macOS 那一侧
（`tauri-2.11.5/src/webview/mod.rs:1008` 的 doc 只对 macOS 标了限制），
Windows 构建时**不该开**它 —— 它会把 macOS 最低版本要求带进来。

### 2. Android WebView 的 `ProxyController` 也支持 `socks5://`

`androidx.webkit.ProxyController.setProxyOverride()` 接受
`addProxyRule("socks5://host:port")`，底层同样是 Chromium 的 proxy config。
两个限制：**进程级全局**（不是 per-WebView，我们只有自己的 WebView，无所谓）、
**不支持认证**（我们的代理本来就无认证）。

⇒ 条带方案三个平台同构。

### 3. 三个平台的抽象点都已存在

| 要补的东西 | 抽象点 | 现有实现 |
|---|---|---|
| 路由 | `RouteBackend` trait（`crates/wsieve-tun/src/managed.rs:49`，只有 `add`/`delete`/`list_managed` 三个方法） | `MacRouteBackend`（:245） |
| 系统代理 | `ManagedSystemState` trait | `SysProxyCustody`（`src-tauri/src/custody/sysproxy.rs:218`） |
| TUN 设备 | `spawn_netstack()`（`crates/wsieve-tun/src/device.rs:66`） | macOS 分支；`:147` 是明确报错的其余平台占位 |

`TunRoutes` 的 bypass 差分、残留清理、启动顺序纪律**全部平台无关**，且已有
`FakeBackend`（`managed.rs:503`）覆盖的测试。新平台只需让那些测试对着新后端
再跑一遍。

### 4. Android 的环路防线比 macOS 简单一个数量级

macOS 上环路（§8.3.1）要靠 `shard_setup` 的 `on_upstream` 钩子在起转发器**之前**
写 bypass 路由，顺序错一步就是静默死循环。

Android 上 `VpnService.Builder.addDisallowedApplication(自己的包名)` 一句话解决：
我们自己的进程（含 WebView）整个被排除在隧道外。EasyTier 就是这么做的，
**全程没有调用 `protect()`**。

⇒ Android 侧 `on_upstream` 钩子传 `None`，`TunRoutes` 那一整套不参与。
**这是 Android 比 Windows 好做的地方**，别照搬 macOS 的复杂度。

### 5. WebView2 / Android WebView 都是 Chromium，不是 WebKit

对项目前提（"TLS 指纹来自真实浏览器引擎"）这是**好事** —— Chrome/Edge 指纹比
Safari 常见得多。但有两处 WebKit 特有的判据必须重验：

- **`ui/emitter.js:56` 的 `canUseRawIpc()`** 判的是 `location.protocol === 'http:'`。
  那条规则的来源是"WKWebView 禁止 https 页面访问 custom scheme"
  （`scripts/spike-ipc-origin.sh` 的四格实测）。**Chromium 没有这条限制**，
  判据在 WebView2 上很可能是错的 —— 而它错的方向最危险（注释里写了：
  开了 RAW 却发不出去 = 心跳正常、传输永久挂起、零错误日志）。
- **h3 协商行为**。`docs` 里记着 "WebKit 升级 h3 是间歇的"；Chromium 更可预测，
  自适应流控的档位可能落在不同的点上。不是 bug，但 A/B 数字要重测。

### 6. 客户端**现在就能在 Windows 上编过**

每个 `cfg(target_os = "macos")` 都配了 `cfg(not(...))` 分支，而且是**明确报错**
而不是静默空转（`device.rs:147`、`sysproxy.rs:194`、`main.rs:701`）。
所以 Phase A 有一个真实可跑的起点，不是从编译错误堆里刨。

---

## Global Constraints

- **绝不在 Rust 里终止 TLS。** 数据面永远由 WebView 直发真实服务端
  （`src-tauri/src/shard.rs:7`）。新增的平台后端只碰路由/代理设置/TUN 设备，
  一个字节的应用层数据都不经手。
- **平台不支持时必须明确报错，不得静默降级。** 这是 §6.4「禁止回退直连」的
  同源纪律，现有的 `not(macos)` 分支已经是这个形状，新写的分支照抄这个态度。
- **`RouteBackend::list_managed` 只能返回自己托管的路由。** 返回值会被原样删掉
  —— 多返回一条就是删用户的路由。Windows 实现必须有独立的记账，不能靠
  「猜哪些像是我们的」。
- **Android 的 fd 所有权只能有一方。** Kotlin 侧 `establish()` 拿到
  `ParcelFileDescriptor` 之后，交给 Rust 的是 `detachFd()` 的裸 int；Kotlin
  **不得**再 `close()`。double-close 是这类集成最常见的崩溃源。
- **不改 xhttp 协议本身。** 三个平台跑同一份 emitter.js 与同一套帧格式。
- **每个平台后端都要有「无权限时报错而不是假装成功」的测试。** Windows 改路由
  要管理员、Android 建 VPN 要用户授权，两者都会失败，而失败被吞掉的后果是
  用户以为流量在隧道里而实际在裸奔。

---

## Phase A：Windows

起点：能编过、能跑混合端口入口，但 TUN 与系统代理会明说"本平台暂不支持"。

### Task A1: 建立 Windows 基线，钉住"现在能做什么"

**Files:**
- Create: `docs/superpowers/plans/2026-09-14-windows-baseline.md`（走查记录，随任务更新）

**Interfaces:**
- Consumes: 无
- Produces: 一份「Windows 上哪些功能已经能用」的实测清单，后续任务据此判断有没有回归

- [ ] **Step 1: 在 Windows 机器上确认能编能跑**

```powershell
rustup target add x86_64-pc-windows-msvc
npm --prefix ui ci
npm --prefix ui run build
# 关键：确认不是占位页（build.rs 会在 ui/dist 缺失时静默写占位页并让编译成功）
Select-String -Path ui/dist/index.html -Pattern "前端尚未构建" -Quiet   # 必须是 False
cargo build --manifest-path src-tauri/Cargo.toml --release --locked
```

注意 `src-tauri/Cargo.toml:64` 的 `macos-proxy` feature：它在非 macOS 上是
no-op 还是编译错误，这一步会给出答案。若报错，改成
`[target.'cfg(target_os = "macos")'.dependencies]` 分平台声明 tauri features。

- [ ] **Step 2: 跑全量测试，记录哪些在 Windows 上失败**

```powershell
cargo test --workspace --locked
cargo test --manifest-path src-tauri/Cargo.toml --bin wsieve-app --locked
```

预期会失败的（都是路径/平台假设，属于本 Phase 要修的）：
- `custody::hosts` 的测试若硬编码了 `/etc/hosts` 风格路径
  （`hosts.rs:26` 的 `system_path()` 已经处理了 Windows，但测试夹具未必）
- `sysproxy` 的 `enumerate_services`（macOS 的 `networksetup` 概念）

**逐条记下来，不要顺手改**——先有清单再动手，否则分不清"本来就不支持"和
"被这次改动弄坏了"。

- [ ] **Step 3: 实测混合端口入口可用**

启动应用，用 curl 走 SOCKS5 与 HTTP 两个入口各打一次，确认代理链路通。
这一步验证的是「Rust 核心在 Windows 上本来就是好的」，为后面三个任务提供
一个已知良好的对照。

---

### Task A2: 多会话条带走 WebView2 的 socks5 代理

**Files:**
- Modify: `src-tauri/Cargo.toml`（`macos-proxy` 改为按平台声明）
- Modify: `src-tauri/src/webview_proxy.rs:483-505`（那段自检 `macos-proxy` 是否开启的断言，要按平台分叉）
- Modify: `src-tauri/src/main.rs:244-256`（`WebviewProxy::spawn` 的注释里"免提权落地件"的平台声明）
- Test: `src-tauri/src/webview_proxy.rs`（同文件 `#[cfg(test)]`）

**Interfaces:**
- Consumes: A1 的基线
- Produces: Windows 上 `Reach::Proxy` 可用 ⇒ Task A6 打包时不需要请求管理员权限只为写 hosts

- [ ] **Step 1: 拆掉 `macos-proxy` 的平台假设**

`src-tauri/Cargo.toml:64` 现在是：

```toml
tauri = { version = "2", features = ["tray-icon", "image-png", "macos-proxy"] }
```

改成：

```toml
tauri = { version = "2", features = ["tray-icon", "image-png"] }

# macos-proxy 只对 macOS 有意义，而且它把最低系统版本抬到 14.0
# （nw_proxy_config_create_* 是 macOS 14 才有的符号）。Windows 侧
# wry 的 WebView2 后端原生支持 socks5 代理（--proxy-server=socks5://），
# 不需要任何 feature——开了反而把 macOS 的版本下限带进 Windows 构建。
[target.'cfg(target_os = "macos")'.dependencies]
tauri = { version = "2", features = ["macos-proxy"] }
```

`webview_proxy.rs:483` 那个「`macos-proxy` 与 macOS 最低版本必须同步」的自检
测试要加 `#[cfg(target_os = "macos")]` —— 它检查的是一条 macOS 专属的不变量，
在 Windows 上跑等于凭空造一个失败。

- [ ] **Step 2: 写测试钉住 Windows 上代理路可用**

`shard_setup.rs` 已有 `an_ip_literal_server_still_gets_multiple_sessions_via_the_proxy`
这类测试，它们不依赖平台（`WebviewProxy::spawn` 只是 bind 一个回环 TcpListener）。
确认它们在 Windows 上全绿即可，**不用新写** —— 真正的 Windows 特有风险在下一步。

- [ ] **Step 3: 真机验证 —— WebView2 确实把请求交给了代理**

这一条不能靠单测，必须实跑：

1. 起应用，让 `webview_proxy` 打开 debug 日志
   （`webview_proxy.rs:182` 那句 `WebView 代理改写: {host}:{port} -> {addr}`）
2. 观察每条会话基址是否都出现了一条改写日志
3. **抓包确认多条 TCP**：Windows 上用 `netstat -ano | findstr <转发端口>`，
   应当看到 `extra_sessions + 1` 条独立连接

**已知风险**：Chromium 对 `--proxy-server` 有一条**隐式的 localhost 绕过**
（不走代理直连回环）。这对我们是无害甚至有利的：数据面目标是
`https://<服务端域名>:<转发端口>`（非回环，照常走代理），而承载页是
`http://127.0.0.1:<壳端口>`（绕过代理直连，本来也是我们想要的）。
但**必须实测确认**，不能推断 —— 若 Chromium 版本变了行为，症状是承载页打不开。

---

### Task A3: 重验 `canUseRawIpc()` 在 WebView2 上的判据

**Files:**
- Modify: `ui/emitter.js:46-58`
- Modify: `ui/emitter.test.js`
- Create: `scripts/spike-ipc-origin-win.ps1`（对照 `scripts/spike-ipc-origin.sh` 的四格实测）

**Interfaces:**
- Consumes: A1 的基线
- Produces: 一条在两个引擎上都正确的 raw-IPC 判据

**为什么这是独立任务而不是顺手改**：现在那条判据
（`location.protocol === 'http:'`）来自 WKWebView 的一条具体限制。Chromium
没有那条限制，所以判据在 Windows 上**要么过于保守**（白白走 base64，吞吐从
114 MB/s 掉到 72 MB/s），**要么过于宽松**（开了 RAW 却发不出去 —— 注释里已经
写明这个方向的症状是"心跳正常、传输永久挂起、零错误日志"）。两种都不能靠猜。

- [ ] **Step 1: 四格实测**

照 `scripts/spike-ipc-origin.sh` 的形状，在 Windows 上把承载页分别放在
`http://127.0.0.1`、`http://localtest.me`、`https://127.0.0.1`、`https://example.com`
四个 origin 下，各试一次 raw IPC（发一个 `Uint8Array` 给 Rust，看 Rust 收到的
是 raw body 还是 JSON）。

- [ ] **Step 2: 按实测结果改判据**

若 Chromium 四格全通，判据变成：

```js
function canUseRawIpc() {
  // WKWebView 禁止 https 页面访问 custom scheme，与 origin 是不是本地无关
  // （四格实测见 scripts/spike-ipc-origin.sh）。Chromium（WebView2 /
  // Android WebView）没有这条限制，四格全通
  // （scripts/spike-ipc-origin-win.ps1）。
  //
  // 判引擎而不是判平台：同一份 emitter.js 要在 WKWebView、WebView2、
  // Android WebView 三处跑，而限制来自引擎。
  var isWebKitOnly = /AppleWebKit/.test(navigator.userAgent)
                     && !/Chrome|Chromium|Edg/.test(navigator.userAgent);
  return !isWebKitOnly || location.protocol === 'http:';
}
```

**若实测不是全通，就按实测写，别按这段猜的写。** 这段代码在此只是形状示意。

- [ ] **Step 3: 补 vitest 用例**

`ui/emitter.test.js` 里已有 IPC 通路的测试夹具。加两组 UA + protocol 的组合，
钉住「Safari + https ⇒ 不用 raw」「Edge + https ⇒ 用 raw」。

---

### Task A4: Windows 系统代理（注册表）

**Files:**
- Modify: `src-tauri/src/custody/sysproxy.rs`（新增 `#[cfg(windows)]` 实现）
- Modify: `src-tauri/Cargo.toml`（加 `windows-sys` 的 Win 平台依赖）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: 无
- Produces: `SysProxyCustody` 在 Windows 上真正生效 ⇒ 用户不开 TUN 也能全局代理

- [ ] **Step 1: 写失败测试**

现有 `ManagedSystemState` 的测试是围绕「apply 之后 state 可读回、revert 之后
恢复原状」写的。加一条 Windows 专属的：写入后从注册表读回 `ProxyEnable` 与
`ProxyServer`，`revert` 之后必须**逐字节**回到原值（包括原来就有代理的情况 ——
那时我们不能把用户的代理设置抹掉）。

- [ ] **Step 2: 实现**

注册表路径：
`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Internet Settings`
- `ProxyEnable`: DWORD，0/1
- `ProxyServer`: REG_SZ，`127.0.0.1:<port>`
- `ProxyOverride`: REG_SZ，建议 `localhost;127.*;<local>`

**改完必须通知系统**，否则已经在跑的进程不会重读：

```rust
// 两次调用缺一不可，这是 WinINet 的既定用法：
//   INTERNET_OPTION_SETTINGS_CHANGED (39) 告诉它"设置变了"
//   INTERNET_OPTION_REFRESH          (37) 让它真的重读
// 只调前者的话，Edge/IE 系的进程要等到下次自己刷新才生效——
// 表现为"开关点了但没反应"，几分钟后又突然生效。
unsafe {
    InternetSetOptionW(None, INTERNET_OPTION_SETTINGS_CHANGED, None, 0);
    InternetSetOptionW(None, INTERNET_OPTION_REFRESH, None, 0);
}
```

用 `windows-sys` 的 `Win32::Networking::WinInet`，不引 `winreg` 之外的新依赖
（`tun-rs` 已经把 `winreg` 带进来了，可以直接用）。

- [ ] **Step 3: 保留 macOS 的「逐条执行不中途 return」纪律**

`sysproxy.rs:200` 的 `run_all` 那段注释说明了为什么关代理时第一条失败也要
继续试完 —— Windows 侧只有三个注册表键，但 revert 路径同样要「全部试完再
一并抛错」，不能第一个键失败就把后两个丢下。


---

### Task A5: Windows TUN —— `WinRouteBackend` + wintun 设备

**Files:**
- Modify: `crates/wsieve-tun/src/managed.rs`（新增 `WinRouteBackend`，`default_gateway` 的 Win 分支）
- Modify: `crates/wsieve-tun/src/device.rs:147`（把「其余平台报错」缩小到 `not(any(macos, windows))`）
- Modify: `src-tauri/src/main.rs:611-701`（`tun_prepare` 的 cfg 分叉）
- Test: `crates/wsieve-tun/src/managed.rs`（复用现有 `FakeBackend` 那批测试的形状）

**Interfaces:**
- Consumes: A1
- Produces: `TunSetup` 在 Windows 上可用 ⇒ 全局模式不再依赖系统代理

**这是 Phase A 里最大的一块，也是唯一需要管理员权限的一块。**

- [ ] **Step 1: `default_gateway()` 的 Windows 实现**

macOS 版（`managed.rs:470`）跑 `/sbin/route -n get default` 再解析。Windows
没有等价的单条命令输出，两个选择：

- **推荐**：`GetBestRoute2`（IPHLPAPI，`windows-sys` 里有）查 `0.0.0.0` 的最佳
  路由，读它的 `NextHop`。结构化返回，不用解析文本。
- 备选：`route print 0.0.0.0` 解析文本 —— **不推荐**，输出随系统语言变
  （中文 Windows 上表头是「网络目标」），解析器会在用户机器上悄悄失配。

取不到网关必须报错，不能猜一个 —— `main.rs:617` 那段注释说明了原因：
「猜错的话每条出站都连不上，而路由表看上去一切正常」。

- [ ] **Step 2: 写失败测试（对着 trait，不碰真实路由表）**

`RouteBackend` 只有三个方法，而 `TunRoutes` 的全部逻辑已被 `FakeBackend`
覆盖。这一步只需要给 `WinRouteBackend` 补它**自己**的测试：

- `list_managed` 只返回自己加的那些（**关键**：全局约束里写了，多返回一条
  就是删用户的路由）
- 无管理员权限时 `add` 返回 `Err` 而不是 `Ok`

第二条要能自动跑：以非管理员身份跑测试时断言 `add` 失败；管理员下跳过
（`#[ignore]` + 说明，照 `sysproxy.rs:576` 那条既有的形状）。

- [ ] **Step 3: 实现 `WinRouteBackend`**

用 `CreateIpForwardEntry2` / `DeleteIpForwardEntry2`（IPHLPAPI），**不要**
shell 出去调 `route.exe`：
- `route.exe` 的输出要解析，且随系统语言变（见 Step 1）
- 它的退出码不区分「已存在」和「没权限」

记账：Windows 上没有 macOS 那种「网关是 TUN 地址」的天然判据可用于
默认路由，因此 `list_managed` **必须**靠自己维护的表。把已添加的
`RouteEntry` 存在 `Mutex<BTreeSet<_>>` 里，并在进程启动时**先做一次残留清理**
—— 上次崩溃留下的 `0.0.0.0/1` + `128.0.0.0/1` 指向一个已经消失的 wintun
适配器，会把半个 IPv4 空间黑洞掉（与 `main.rs:605` 那段 macOS 的理由完全相同）。

残留识别：按 wintun 适配器的 LUID 找 —— 适配器名是我们建的，
`GetIpForwardTable2` 里按 `InterfaceLuid` 过滤，比记账文件可靠
（记账文件会随崩溃一起丢）。

- [ ] **Step 4: `spawn_netstack()` 的 Windows 分支**

`tun-rs` 的 Windows 后端靠 **`wintun.dll`**（用 `libloading` 动态加载）。
两件事：

1. **DLL 必须与 exe 同目录**。从 <https://www.wintun.net/> 取官方签名的
   `wintun.dll`（`bin/amd64/wintun.dll` 与 `bin/arm64/`）。**不要**编译自己的
   —— 官方 DLL 有 WHQL 签名，自编的在开了安全启动的机器上装不上驱动。
2. 设备参数与 macOS 分支对齐（`device.rs:66` 那段）：

```rust
#[cfg(target_os = "windows")]
pub async fn spawn_netstack() -> std::io::Result<NetStack> {
    let builder = DeviceBuilder::new()
        .name("websieve")            // Windows 需要显式名字，用它来认自己的适配器
        .ipv4(TUN_ADDR, TUN_PREFIX, None)
        .mtu(TUN_MTU);
    // 不设 packet_information：Windows 的 wintun 本来就给裸 IP 包
    // （那个开关是 macOS utun 的 4 字节头，Windows 上没有对应物）。
    // 同样不用 associate_route(false)：路由全部由 WinRouteBackend 托管。
    let dev = Arc::new(builder.build_async()?);
    // ...其余与 macOS 分支同构，抽成共享函数避免两边漂移
}
```

**macOS 与 Windows 分支里 netstack 的接线部分（`StackBuilder` 那一段）应当
抽成一个平台无关的函数**，只让 `DeviceBuilder` 的构造分叉。两份复制粘贴的
netstack 接线迟早会在只改一处时静默分歧。

- [ ] **Step 5: 真机走查**

照 `docs/superpowers/plans/2026-08-25-phase6-tun.md` 里 M1–M9 的形状，在
Windows 上重跑一遍。**最重要的三条**：

- M-环路：开 TUN 之后出站仍然连得上（说明 bypass 路由生效了）
- M-残留：`taskkill /F` 杀掉进程后重启，旧路由被清掉（不是叠加）
- M-无权限：以普通用户身份启动并开 TUN，必须看到明确报错，而**不是**
  「界面显示已开启但流量在裸奔」

---

### Task A6: Windows 打包

**Files:**
- Modify: `src-tauri/tauri.conf.json`（`bundle.active`、`targets`、`resources`）
- Create: `src-tauri/wintun/`（放 `wintun.dll`，或在构建脚本里下载）

**Interfaces:**
- Consumes: A2–A5
- Produces: 可分发的 `.msi` / `.exe`

- [ ] **Step 1: 打开 bundle 并带上 wintun.dll**

```json
"bundle": {
  "active": true,
  "targets": ["nsis", "msi", "app", "dmg"],
  "resources": { "wintun/amd64/wintun.dll": "wintun.dll" },
  "macOS": { "minimumSystemVersion": "14.0" }
}
```

`resources` 的值是**目标路径**：DLL 要落在 exe 同目录，`libloading` 才找得到。

图标：`src-tauri/icons/` 现在只有 `icon.png` 与 `icon.ico`，bundle 需要更多
尺寸。用 `cargo tauri icon src-tauri/icons/icon.png` 生成（不入库，构建时现生成）。

- [ ] **Step 2: 决定要不要 UAC 提权清单**

**建议不要默认提权。** 理由：
- 条带走 WebView2 代理（A2），不需要写 hosts ⇒ 不需要管理员
- 系统代理写的是 `HKEY_CURRENT_USER`（A4）⇒ 不需要管理员
- **只有 TUN（A5）需要**

所以让主程序以普通权限跑，开 TUN 时才提权 —— 要么引导用户「以管理员身份
重新启动」，要么把 TUN 那部分拆成一个提权的辅助进程。默认 `requireAdministrator`
会让每次启动都弹 UAC，而绝大多数用户不开 TUN。

**这一步需要你在实现前决定走哪条**，两条路的工作量差很多（辅助进程要设计 IPC）。


---

## Phase B：Android（VPN 模式）

**与 Windows 的根本差别**：Windows 是「填三个已有抽象点的空」，Android 是
「新增一个平台外壳 + 一条 Kotlin↔Rust 的 fd 通道」。Rust 核心不变，但
`src-tauri` 那一层有相当一部分是桌面专属的（托盘、系统代理、hosts），
必须先按平台切干净。

**建议在 Phase A 完成之后再开始** —— A3（raw IPC 判据）与 A2（Chromium 代理）
的结论 Android 直接复用，先做 Windows 等于免费把 Chromium 那条路趟平。

### Task B1: Android 脚手架 + 让现有代码编过

**Files:**
- Create: `src-tauri/gen/android/`（`cargo tauri android init` 生成）
- Modify: `src-tauri/src/main.rs`（托盘、系统代理、hosts 的调用点按平台切）
- Modify: `src-tauri/Cargo.toml`（`tray-icon` 改为桌面专属）

**Interfaces:**
- Consumes: 无
- Produces: 一个能装到真机上、能开控制界面的 APK（此时还没有 VPN）

- [ ] **Step 1: 初始化**

```bash
cargo install tauri-cli --version "^2" --locked
cd src-tauri && cargo tauri android init
```

需要 Android SDK/NDK 与 `ANDROID_HOME`/`NDK_HOME`。生成物**要入库**
（`gen/android/` 里有我们要手改的 Kotlin 与 manifest）。

- [ ] **Step 2: 把桌面专属的东西按平台切开**

三处，都是「Android 上没有对应概念」而不是「暂未实现」，所以**不要**照
`not(macos)` 那样写成运行时报错 —— 那会在 Android 上刷出永远无法解决的错误
日志。正确形状是编译期切掉 + 界面上不显示对应开关：

| 模块 | Android 上的处理 |
|---|---|
| `tray.rs` | `#[cfg(desktop)]`。托盘 API 本来就挂在 `#[cfg(all(desktop, feature = "tray-icon"))]` 下（见 `tray.rs:8`） |
| `custody/sysproxy.rs` | `#[cfg(desktop)]`。Android 的等价物就是 VPN 本身，没有「系统代理」这个东西 |
| `custody/hosts.rs` | `#[cfg(desktop)]`。非 root 改不了 `/system/etc/hosts`；条带走 `ProxyController`（B4） |

`Cargo.toml` 的 `tray-icon` 也要改成桌面专属，否则 Android 构建会拉一堆
用不上的桌面依赖。

- [ ] **Step 3: 装到真机，确认控制界面能开**

```bash
cargo tauri android dev      # 或 android build --apk
```

此时 VPN 与条带都还没有，验的是「Tauri 外壳 + Svelte 界面 + Rust 核心
在 Android 上跑得起来」。混合端口入口应当能起（Android 上监听本地端口
不需要权限），可以用同机的另一个 app 或 `adb shell curl` 验一次。

---

### Task B2: VpnService 插件（Kotlin 侧）

**Files:**
- Create: `src-tauri/gen/android/app/src/main/java/<pkg>/WsieveVpnService.kt`
- Create: `src-tauri/gen/android/app/src/main/java/<pkg>/VpnPlugin.kt`
- Modify: `src-tauri/gen/android/app/src/main/AndroidManifest.xml`

**Interfaces:**
- Consumes: B1
- Produces: 一个 Tauri 插件事件 `vpn_service_start`，payload 是 `{ fd: i32 }`

**照 EasyTier 的 `tauri-plugin-vpnservice` 的形状走**，但**大幅简化**：
它要支持任意 CIDR 路由与多实例，我们只有一条隧道、一份固定配置。

- [ ] **Step 1: manifest**

```xml
<uses-permission android:name="android.permission.INTERNET"/>
<uses-permission android:name="android.permission.FOREGROUND_SERVICE"/>
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_SPECIAL_USE"/>
<uses-permission android:name="android.permission.POST_NOTIFICATIONS"/>

<service
    android:name=".WsieveVpnService"
    android:permission="android.permission.BIND_VPN_SERVICE"
    android:foregroundServiceType="specialUse"
    android:exported="false">
    <intent-filter><action android:name="android.net.VpnService"/></intent-filter>
    <property
        android:name="android.app.PROPERTY_SPECIAL_USE_FGS_SUBTYPE"
        android:value="VPN tunnel for the user's own proxy configuration"/>
</service>
```

Android 14+ 强制要求 `foregroundServiceType` 与对应权限，漏了会在启动前台
服务时直接抛 `SecurityException`。

- [ ] **Step 2: `WsieveVpnService.kt`**

```kotlin
private fun createVpnInterface(): ParcelFileDescriptor {
    val builder = Builder()
        .setSession("websieve")
        .setBlocking(false)
        .addAddress("10.126.126.1", 24)
        .setMtu(1500)
        .addRoute("0.0.0.0", 0)          // 全局接管
        .addDnsServer("198.18.0.1")      // fake-ip 段，与 wsieve-dns 对齐

    // **环路防线，一句话解决**：把自己整个进程排除在隧道外。
    //
    // macOS 上这件事要靠 shard_setup 的 on_upstream 钩子在起转发器之前写
    // bypass 路由（§8.3.1），顺序错一步就是静默死循环。Android 有原生 API，
    // 别照搬那套复杂度。
    //
    // 排除的是「我们自己」，也就是承载 WebView 的这个进程——数据面本来就
    // 该直连真实服务端，不该再回到隧道里绕一圈。
    builder.addDisallowedApplication(packageName)

    return builder.establish() ?: throw IllegalStateException("建立 VPN 失败")
}
```

**fd 交接**（全局约束里点名的 double-close 风险）：

```kotlin
val pfd = createVpnInterface()
val data = JSObject()
// detachFd() 之后这个 ParcelFileDescriptor 不再拥有 fd——Kotlin 侧
// 绝对不能再 close() 它。所有权整个转给 Rust。
data.put("fd", pfd.detachFd())
triggerCallback("vpn_service_start", data)
```

- [ ] **Step 3: 生命周期**

三件必做的事，漏一件都会表现为「VPN 莫名其妙断了」：

1. **`onRevoke()`** —— 用户在系统设置里掐断 VPN 会调它。必须通知 Rust 收摊，
   否则 Rust 还抱着一个已经失效的 fd 在读。
2. **前台通知** —— 不发通知的前台服务会在几秒内被系统杀掉。
3. **`VpnService.prepare()` 授权流程** —— 首次启动要弹系统确认框，
   `onActivityResult` 拿到 `RESULT_OK` 才能 `startService`。

---

### Task B3: Rust 侧接管外部 fd

**Files:**
- Modify: `crates/wsieve-tun/src/device.rs`（新增 `spawn_netstack_from_fd`）
- Modify: `src-tauri/src/main.rs`（Android 的 `tun_prepare` 分支）
- Create: `src-tauri/src/commands/vpn.rs`（`set_tun_fd` 命令）

**Interfaces:**
- Consumes: B2 的 `vpn_service_start` 事件
- Produces: TUN 数据面在 Android 上跑起来

- [ ] **Step 1: `spawn_netstack_from_fd`**

`tun_rs::AsyncDevice::from_fd(RawFd)` 是现成的（`tun-rs-2.8.8/src/platform/mod.rs:165`，
`unsafe`）。Android 分支**不建设备、不设地址、不写路由** —— 那些
Kotlin 的 `Builder` 已经做完了：

```rust
/// 接管由外部（Android VpnService）建立好的 TUN fd。
///
/// 与 `spawn_netstack()` 的分工：那个函数**建**设备，这个函数**接**设备。
/// Android 上地址、MTU、路由、DNS 全部由 Kotlin 侧的 VpnService.Builder
/// 设好了，Rust 只负责收发包——重复设置不是无害的，会直接失败。
///
/// # Safety
/// `fd` 必须是一个有效的、**所有权已完整转移**的 TUN fd。调用方之后不得
/// 再 close 它（Kotlin 侧用 `detachFd()` 正是为此）。
#[cfg(target_os = "android")]
pub async unsafe fn spawn_netstack_from_fd(fd: std::os::fd::RawFd) -> std::io::Result<NetStack> {
    let dev = Arc::new(tun_rs::AsyncDevice::from_fd(fd)?);
    // 之后与 macOS 分支共用同一段 netstack 接线（见 Task A5 Step 4 里
    // 抽出来的那个平台无关函数）
}
```

- [ ] **Step 2: 命令与事件接线**

流程照 EasyTier：Kotlin 发事件 → 前端 `addPluginListener` 收到 →
`invoke('set_tun_fd', { fd })` → Rust 接管。

**为什么绕前端一圈而不是 Kotlin 直接调 Rust**：Tauri 插件的事件通道本来就
接到前端，而前端此刻需要知道 VPN 状态（要更新界面）。走一圈让状态只有
一个来源，比 Kotlin 与前端各自维护一份少一类 bug。

- [ ] **Step 3: Android 的 `tun_prepare` —— 显式地什么都不做**

```rust
/// Android：路由与 bypass 全部由 VpnService 负责，Rust 侧不参与。
///
/// **这不是"尚未实现"**，是这个平台上正确的做法：
/// `addDisallowedApplication(自己)` 已经把承载 WebView 排除在隧道外，
/// 比 macOS 那套逐 IP 写 bypass 路由既简单又不会漏。
/// 因此 TunRoutes / RouteBackend / default_gateway 一个都不用。
#[cfg(target_os = "android")]
fn tun_prepare(_cfg: &bootstrap::AppConfig, _server_urls: &[String]) -> Option<TunSetup> {
    None
}
```

⚠️ 但要确认一件事：`shard_setup::plan_many` 的 `on_upstream` 钩子在
`tun_may_start()` 判据里是有份量的（`ShardPlanEntry::tun_may_start`）。
Android 上没有 bypass 也要能开 TUN，所以那条门禁要按平台放行 ——
**别把它整个删掉**，macOS/Windows 上它仍然是环路的唯一防线。

---

### Task B4: Android 的多会话条带

**Files:**
- Create: Kotlin 侧 `setProxyOverride` 的调用（可并入 B2 的插件）
- Modify: `src-tauri/src/shard_setup.rs`（`Reach` 加 Android 分支）

**Interfaces:**
- Consumes: B1、`src-tauri/src/webview_proxy.rs`（原样复用）
- Produces: Android 上也能多 TCP

- [ ] **Step 1: 用 `ProxyController` 指向本地 SOCKS5**

`webview_proxy` 模块**完全不用改** —— 它只是一个本地 SOCKS5 监听器。
变的只是"怎么告诉 WebView 用它"：

```kotlin
if (WebViewFeature.isFeatureSupported(WebViewFeature.PROXY_OVERRIDE)) {
    val config = ProxyConfig.Builder()
        // 必须写 socks5:// 而不是 socks://——后者在 Chromium 的
        // proxy-resolution 规则里历史上映射到 SOCKS4。
        .addProxyRule("socks5://127.0.0.1:$port")
        .build()
    ProxyController.getInstance().setProxyOverride(config, executor) { /* ready */ }
}
```

两个必须知道的限制：
- **进程级全局**，不是 per-WebView。我们只有自己的 WebView，无所谓。
- **`addBypassRule` 对 socks 可能被忽略**。我们依赖的是 Chromium 对回环的
  隐式绕过（与 Windows 同一条，见 A2 Step 3），**同样要实测确认**。

- [ ] **Step 2: 时序**

`setProxyOverride` 是**异步**的（回调 ready）。必须在**加载承载页之前**
完成 —— 与桌面端「代理必须先于建窗口起来」（`main.rs:244`）是同一条纪律，
只是 Android 上要额外等那个回调。

- [ ] **Step 3: 实测多 TCP**

`adb shell ss -tn | grep <转发端口>`，应当看到 `extra_sessions + 1` 条。
条带在 Android 上要是不成立，退回单会话也能用 —— 但**必须在日志里说明**，
不能静默。

---

### Task B5: Android 打包与真机走查

- [ ] **Step 1: 出 APK**

```bash
cargo tauri android build --apk --target aarch64
```

- [ ] **Step 2: 走查清单（每条都要在真机上过）**

| # | 项目 | 判据 |
|---|---|---|
| 1 | 首次开 VPN | 弹系统授权框；拒绝后有明确提示而不是静默失败 |
| 2 | 环路 | 开 VPN 之后出站仍然连得通（`addDisallowedApplication` 生效） |
| 3 | 前台通知 | 常驻通知在；息屏 10 分钟后 VPN 仍在 |
| 4 | `onRevoke` | 从系统设置掐断 VPN，应用界面状态同步变成"已断开" |
| 5 | fd 所有权 | 反复开关 VPN 10 次不崩（double-close 会在这里现形） |
| 6 | 条带 | `ss -tn` 看到多条 TCP |
| 7 | raw IPC | 大文件下载吞吐接近 Windows 侧的数字（差一截说明退化到 base64 了） |

---

## 分阶段验收

**Phase A 完成的标志**：Windows 上四项全部可用 —— 混合端口入口、多会话条带、
系统代理、TUN，且 TUN 无权限时明确报错。

**Phase B 完成的标志**：Android 上 VPN 能接管全局流量、应用自身流量不入隧道、
反复开关不崩。条带成立是加分项，不成立要有日志。

## 建议顺序

```
A1 → A2 → A3 ─┬→ A4 ─┐
              └→ A5 ─┴→ A6        （A4/A5 互不依赖，可并行）
                        ↓
                  B1 → B2 → B3 → B4 → B5
```

A3 的结论（Chromium 上 raw IPC 判据）Android 直接复用，所以**先做完 Windows
再开 Android** 能省掉一整轮四格实测。

## 不做

- **iOS**：`NEPacketTunnelProvider` 要 Network Extension entitlement + 付费
  开发者账号，且必须拆成独立的 extension 进程 —— 与本计划的架构差异远大于
  Android，值得单独一份计划。
- **Linux 桌面**：TUN 要 netlink 路由实现，而 WebKitGTK 的指纹与 Chrome/Safari
  都不像，反而更显眼。没有明确需求前不做。
- **Windows ARM 的 wintun**：官方 DLL 有 arm64 版，但没有机器实测就不宣称支持。
  A6 只打 amd64，arm64 等有机器再说。

// 跨域名承载 spike 的 WebView 驱动（配合 examples/cross_origin_spike.rs）。
//
// 用真实 WKWebView —— 与 Tauri 在 macOS 上用的是同一个 WebKit 网络栈 ——
// 加载域名 A 上的驱动页。页面自己会对 A 与 B 两个 origin 发 fetch，
// 其中对 B 的是被测的 cross-site 请求。
//
// 关键：只建**一个** WKWebView。这正是 `carrier: shared` 的形态。
// 若要对照 isolated，把 SPIKE_WEBVIEWS 设为 2（各自加载各自的 origin）。
//
// 无需 GUI 窗口：setActivationPolicy(.prohibited) 下 WKWebView 仍能完成
// 导航与 fetch（已实测）。因此本 spike 可在纯 SSH 会话里跑。

import AppKit
import WebKit

let app = NSApplication.shared
app.setActivationPolicy(.prohibited)

let driverURL = ProcessInfo.processInfo.environment["SPIKE_DRIVER_URL"]
    ?? "http://localtest.me:18081/__spike/driver"
let seconds = Double(ProcessInfo.processInfo.environment["SPIKE_SECONDS"] ?? "90") ?? 90

final class Nav: NSObject, WKNavigationDelegate, WKScriptMessageHandler {
    var loaded = false
    func webView(_ w: WKWebView, didFinish n: WKNavigation!) {
        FileHandle.standardError.write("[wkdriver] 驱动页加载完成\n".data(using: .utf8)!)
        loaded = true
    }
    func webView(_ w: WKWebView, didFail n: WKNavigation!, withError e: Error) {
        FileHandle.standardError.write("[wkdriver] 导航失败: \(e)\n".data(using: .utf8)!)
    }
    func webView(_ w: WKWebView, didFailProvisionalNavigation n: WKNavigation!, withError e: Error) {
        FileHandle.standardError.write("[wkdriver] 导航失败(prov): \(e)\n".data(using: .utf8)!)
    }
    // 页面 console.log 转发到 stderr，便于排障（绝不静默吞错）
    func userContentController(_ c: WKUserContentController, didReceive m: WKScriptMessage) {
        FileHandle.standardError.write("[page] \(m.body)\n".data(using: .utf8)!)
    }
}

let cfg = WKWebViewConfiguration()
// 与 Tauri 一致：允许对本地 HTTP 的普通加载（无特殊放宽）。
let nav = Nav()
let ucc = WKUserContentController()
ucc.add(nav, name: "log")
ucc.addUserScript(WKUserScript(
    source: """
    (function(){
      var o = console.log;
      console.log = function(){ try{ window.webkit.messageHandlers.log.postMessage(
        Array.prototype.join.call(arguments,' ')); }catch(e){} o.apply(console, arguments); };
      window.onerror = function(m,s,l,c,e){ try{ window.webkit.messageHandlers.log.postMessage(
        'ERROR '+m+' @'+s+':'+l); }catch(e2){} };
    })();
    """,
    injectionTime: .atDocumentStart,
    forMainFrameOnly: false))
cfg.userContentController = ucc

let wv = WKWebView(frame: .zero, configuration: cfg)
wv.navigationDelegate = nav

guard let u = URL(string: driverURL) else {
    FileHandle.standardError.write("[wkdriver] 非法 URL: \(driverURL)\n".data(using: .utf8)!)
    exit(2)
}
FileHandle.standardError.write("[wkdriver] 加载 \(driverURL)\n".data(using: .utf8)!)
wv.load(URLRequest(url: u))

// 常驻跑 runloop：页面的 fetch 全在这段时间里发生。
let deadline = Date().addingTimeInterval(seconds)
while Date() < deadline {
    RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.1))
}
FileHandle.standardError.write("[wkdriver] 到时退出\n".data(using: .utf8)!)
exit(0)

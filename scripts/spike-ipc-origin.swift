// Spike：`http://127.0.0.1:PORT` 这种普通 http origin 下，Tauri 的 raw body
// 快路径（custom protocol IPC）到底能不能用？
//
// 背景：2026-09-09 实测确证承载页是**远程 https origin** 时，
// `fetch('ipc://localhost/…')` 被 WKWebView 整个挡在发出之前，Tauri 静默回退
// 到 postMessage，二进制只能走 base64（实测代价：48.8 MB/s @CPU 86%，
// 而 raw 是 114.9 MB/s @CPU 34%）。
//
// 若把承载页换成本地 origin 就能拿回 raw，那 base64 就能删掉。但
// `emitter.js:236` 那句「本地 origin 一定能走 raw」是**推断**——它成立于
// Tauri 自己的 asset protocol 页（`tauri://localhost`，本身就是 custom
// scheme），而我们要用的是一个普通的 `http://127.0.0.1:PORT`。对 WebKit
// 来说后者是**普通 web origin**，与 https 页面可能是同一类待遇。
//
// 本探针不跑 Tauri，直接复现 Tauri 在 macOS 上的机制：注册一个
// `WKURLSchemeHandler` 处理 `ipc` scheme（wry 就是这么做的），然后让页面
// 用完全一样的方式去 fetch 它。
//
// **对照实验**：同一段注入脚本跑在两个 origin 上，唯一变量是 scheme。
//   - 对照组 `https://example.com/`  —— 已知应当失败（生产现状）
//   - 实验组 `http://127.0.0.1:PORT/` —— 待测
// 对照组若也成功，说明探针没能复现 Tauri 的限制，本 spike 无效，
// 结论一律不采信。

import AppKit
import WebKit

let app = NSApplication.shared
app.setActivationPolicy(.prohibited)

let targetURL = ProcessInfo.processInfo.environment["PROBE_URL"] ?? "http://127.0.0.1:18099/"
let seconds = Double(ProcessInfo.processInfo.environment["PROBE_SECONDS"] ?? "20") ?? 20

func err(_ s: String) {
    FileHandle.standardError.write((s + "\n").data(using: .utf8)!)
}

/// 复现 Tauri/wry 在 macOS 上的 IPC 通路：一个自定义 scheme 的 handler。
///
/// 返回 `Access-Control-Allow-Origin: *` 与 Tauri 一致——这样若请求失败，
/// 就一定不是 CORS 的锅，而是 scheme 本身被封（这正是要区分的两件事）。
final class IpcHandler: NSObject, WKURLSchemeHandler {
    var hits = 0
    /// fetch 的 body 在 custom scheme handler 里可能以 httpBody 或
    /// httpBodyStream 两种形态出现，两个都读——只报告事实，不猜。
    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        hits += 1
        let req = task.request
        let direct = req.httpBody?.count ?? -1
        var streamed = -1
        if let s = req.httpBodyStream {
            s.open()
            var buf = [UInt8](repeating: 0, count: 4096)
            var total = 0
            while s.hasBytesAvailable {
                let n = s.read(&buf, maxLength: buf.count)
                if n <= 0 { break }
                total += n
            }
            s.close()
            streamed = total
        }
        err("[ipc-handler] 命中 #\(hits) method=\(req.httpMethod ?? "?") "
            + "httpBody=\(direct)B httpBodyStream=\(streamed)B "
            + "ct=\(req.value(forHTTPHeaderField: "Content-Type") ?? "-")")

        let resp = HTTPURLResponse(
            url: req.url!,
            statusCode: 200,
            httpVersion: "HTTP/1.1",
            headerFields: [
                "Access-Control-Allow-Origin": "*",
                "Content-Type": "application/json",
            ])!
        task.didReceive(resp)
        let payload = "{\"httpBody\":\(direct),\"httpBodyStream\":\(streamed)}"
        task.didReceive(payload.data(using: .utf8)!)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {}
}

final class Nav: NSObject, WKNavigationDelegate, WKScriptMessageHandler {
    var done = false
    func webView(_ w: WKWebView, didFinish n: WKNavigation!) {
        err("[probe] 页面加载完成")
    }
    func webView(_ w: WKWebView, didFail n: WKNavigation!, withError e: Error) {
        err("[probe] 导航失败: \(e)")
        done = true
    }
    func webView(_ w: WKWebView, didFailProvisionalNavigation n: WKNavigation!, withError e: Error) {
        err("[probe] 导航失败(prov): \(e)")
        done = true
    }
    /// 接受自签证书。**只为 spike 服务**：要测「本地 + https」这一格，
    /// 就得有一个本地 https server，而它必然是自签的。生产代码里没有、
    /// 也绝不该有这个放宽。
    func webView(_ w: WKWebView,
                 didReceive challenge: URLAuthenticationChallenge,
                 completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        if let t = challenge.protectionSpace.serverTrust {
            completionHandler(.useCredential, URLCredential(trust: t))
        } else {
            completionHandler(.performDefaultHandling, nil)
        }
    }

    /// 两问各自的结果。两个都到齐才收工——只等其中一个会让另一个的 fetch
    /// 被进程退出打断，报出来的「失败」就不是被测行为而是我们自己掐的。
    var gotIpc = false
    var gotMixed = false

    func userContentController(_ c: WKUserContentController, didReceive m: WKScriptMessage) {
        let s = "\(m.body)"
        // 结果行走 stdout（供脚本判定），其余诊断走 stderr。
        if s.hasPrefix("PROBE_RESULT") || s.hasPrefix("PROBE_ORIGIN") || s.hasPrefix("PROBE_MIXED") {
            FileHandle.standardOutput.write((s + "\n").data(using: .utf8)!)
            if s.hasPrefix("PROBE_RESULT") { gotIpc = true }
            if s.hasPrefix("PROBE_MIXED") { gotMixed = true }
            done = gotIpc && gotMixed
        } else {
            err("[page] \(s)")
        }
    }
}

let ipc = IpcHandler()
let cfg = WKWebViewConfiguration()
cfg.setURLSchemeHandler(ipc, forURLScheme: "ipc")

let nav = Nav()
let ucc = WKUserContentController()
ucc.add(nav, name: "log")

// 注入脚本 = Tauri 的 initialization_script 的等价物。两个 origin 用的是
// **同一段**代码，这是对照实验成立的前提。
ucc.addUserScript(WKUserScript(
    source: """
    (function () {
      function log(s) { try { window.webkit.messageHandlers.log.postMessage(String(s)); } catch (e) {} }
      log('PROBE_ORIGIN ' + location.origin);
      window.addEventListener('error', function (e) { log('window.onerror ' + e.message); });
      // 第二问：承载页变成 http 之后，它还能不能 fetch **https** 端点？
      // 整个方案押在这一条上——数据面必须留在 https 才有真实 TLS 指纹。
      // mixed content 理应只拦 https→http 的降级方向，但没测就是推断。
      // 用一个已知回 `Access-Control-Allow-Origin: *` 的端点，把 CORS
      // 这个变量排除掉，单独看 scheme 升级方向通不通。
      (async function () {
        var url = 'https://api.github.com/';
        try {
          var r = await fetch(url, { method: 'GET' });
          log('PROBE_MIXED ok status=' + r.status);
        } catch (e) {
          log('PROBE_MIXED fail ' + (e && e.name) + ': ' + (e && e.message));
        }
      })();

      // 复现 emitter 的 raw 回帧：POST 一个 Uint8Array，Content-Type 为
      // application/octet-stream —— 与 Tauri invoke 走 custom protocol 时
      // 发出的请求形态一致。
      (async function () {
        var body = new Uint8Array([0x57, 0x53, 0x49, 0x45, 1, 2, 3, 4]);
        try {
          var r = await fetch('ipc://localhost/wsieve_raw_post', {
            method: 'POST',
            headers: { 'Content-Type': 'application/octet-stream' },
            body: body
          });
          var t = await r.text();
          log('PROBE_RESULT ok status=' + r.status + ' reply=' + t);
        } catch (e) {
          log('PROBE_RESULT fail ' + (e && e.name) + ': ' + (e && e.message));
        }
      })();
    })();
    """,
    injectionTime: .atDocumentEnd,
    forMainFrameOnly: true))
cfg.userContentController = ucc

let wv = WKWebView(frame: .zero, configuration: cfg)
wv.navigationDelegate = nav

guard let u = URL(string: targetURL) else {
    err("[probe] 非法 URL: \(targetURL)")
    exit(2)
}
err("[probe] 加载 \(targetURL)")
wv.load(URLRequest(url: u))

let deadline = Date().addingTimeInterval(seconds)
while Date() < deadline && !nav.done {
    RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05))
}
if !nav.done {
    // 超时本身是信息：既没成功也没抛错，多半是导航就没起来。
    FileHandle.standardOutput.write("PROBE_RESULT timeout\n".data(using: .utf8)!)
}
// 让 handler 的最后一条日志有机会刷出去。
err("[probe] handler 命中次数 = \(ipc.hits)")
exit(0)

import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  // configFile 是相对于 vite root（即 src/）解析的，而 svelte.config.js 按计划
  // 放在 ui/ 根。不显式指向就会退回默认配置且只打一行警告 —— 预处理器静默失效，
  // 属于房规不接受的静默错误，故写死路径。
  plugins: [svelte({ configFile: '../svelte.config.js' })],
  // 源码放 src/，产物出到 ui/dist —— tauri.conf.json 的 frontendDist 指向后者
  root: 'src',
  publicDir: false,
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    // 控制窗口是 WKWebView（macOS）/ WebView2（Windows）/ WebKitGTK（Linux）。
    // 目标定在 safari15 而非默认的 baseline：只有一个已知的运行环境，
    // 没必要为不存在的旧浏览器付出降级代码的体积。
    target: 'safari15',
    sourcemap: false,
  },
  server: { port: 5174, strictPort: true },
  // Vite 默认会清屏，把 cargo 的编译输出冲掉
  clearScreen: false,
});

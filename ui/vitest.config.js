import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// 测试用的独立配置，不复用 vite.config.js —— 后者把 root 设成了 src/，
// 而测试文件与被测模块同目录、又要能引到 ui/ 根下的 svelte.config.js，
// 两套 root 混在一起只会互相绊住。
export default defineConfig({
  // 不传 { hot: false }：plugin-svelte 7.x 已移除该选项，传了会打印
  // `invalid plugin option 'hot' in inline config` —— 房规不接受静默/半静默的配置错误。
  plugins: [svelte()],
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test-setup.js'],
  },
  // 组件测试要拿到 svelte 的 browser 版入口，否则 mount 行为不对
  resolve: { conditions: ['browser'] },
});

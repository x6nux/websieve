// vitest 的全局 setup。jest-dom 的自定义断言（toBeInTheDocument 等）
// 要在每个测试文件之前注册一次。
import '@testing-library/jest-dom/vitest';

/**
 * jsdom 没有实现 `window.matchMedia`（jsdom#2781，至今未做）。
 *
 * 这不是可以绕开的细节：Svelte 的 `prefersReducedMotion` 在模块顶层就
 * `new MediaQuery(...)`，构造函数里直接调 `window.matchMedia` —— 缺了它，
 * 任何 import 了 `svelte/motion` 的组件在**加载期**就抛 TypeError，
 * 测试连一条都跑不起来。
 *
 * 下面补的是环境缺失的浏览器 API，不是对被测代码的 mock：它按真实的
 * MediaQueryList 契约实现，默认全部不匹配（等同于用户没开启任何辅助功能偏好）。
 * 要断言 reduced-motion 行为的测试可以调 `setMediaMatches(fn)` 换掉判定函数。
 */
let mediaMatcher = () => false;

/** 换掉媒体查询的判定。传入 (query) => boolean。 */
export function setMediaMatches(fn) {
  mediaMatcher = fn;
}

if (typeof window !== 'undefined' && typeof window.matchMedia !== 'function') {
  window.matchMedia = (query) => {
    const listeners = new Set();
    return {
      media: query,
      get matches() {
        return mediaMatcher(query);
      },
      onchange: null,
      addEventListener: (type, fn) => {
        if (type === 'change') listeners.add(fn);
      },
      removeEventListener: (type, fn) => {
        if (type === 'change') listeners.delete(fn);
      },
      // 已废弃但仍有库在用，一并提供以免踩到静默失败
      addListener: (fn) => listeners.add(fn),
      removeListener: (fn) => listeners.delete(fn),
      dispatchEvent: (e) => {
        for (const fn of listeners) fn(e);
        return true;
      },
    };
  };
}

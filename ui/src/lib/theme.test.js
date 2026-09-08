import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import {
  getThemePreference,
  setThemePreference,
  initTheme,
  THEME_KEY,
} from './theme.js';

/** 模拟 matchMedia：`matches` 由调用方传入，返回的对象支持
 *  addEventListener('change', cb) / removeEventListener 记录监听器，
 *  测试里手动调用 fireSystemChange 模拟系统切换。 */
function mockMatchMedia(initialMatches) {
  let matches = initialMatches;
  const listeners = new Set();
  const mql = {
    get matches() { return matches; },
    media: '(prefers-color-scheme: dark)',
    addEventListener: (_type, cb) => listeners.add(cb),
    removeEventListener: (_type, cb) => listeners.delete(cb),
  };
  window.matchMedia = vi.fn(() => mql);
  return {
    fireSystemChange(next) {
      matches = next;
      for (const cb of listeners) cb({ matches: next });
    },
    listenerCount: () => listeners.size,
  };
}

// mockMatchMedia() 用裸赋值换掉 window.matchMedia（vi.restoreAllMocks 不会撤销
// 裸赋值，只对 vi.spyOn 生效），所以在这里显式存一份原值（test-setup.js 装的
// polyfill），每个测试后自己恢复，而不是依赖 Vitest 按文件隔离全局对象这个隐式前提。
let originalMatchMedia;

beforeEach(() => {
  originalMatchMedia = window.matchMedia;
  localStorage.clear();
  document.documentElement.removeAttribute('data-theme');
});

afterEach(() => {
  window.matchMedia = originalMatchMedia;
  vi.restoreAllMocks();
});

describe('getThemePreference / setThemePreference', () => {
  it('未设置时回退到 system', () => {
    expect(getThemePreference()).toBe('system');
  });

  it('非法值回退到 system', () => {
    localStorage.setItem(THEME_KEY, '这不是合法的主题值');
    expect(getThemePreference()).toBe('system');
  });

  it('setThemePreference 写入 localStorage 且返回写入的值', () => {
    expect(setThemePreference('light')).toBe('light');
    expect(localStorage.getItem(THEME_KEY)).toBe('light');
    expect(getThemePreference()).toBe('light');
  });

  it('setThemePreference 立即把 data-theme 应用到 <html>', () => {
    mockMatchMedia(false);
    setThemePreference('light');
    expect(document.documentElement.dataset.theme).toBe('light');
    setThemePreference('dark');
    expect(document.documentElement.dataset.theme).toBe('dark');
  });
});

describe('跟随系统', () => {
  it("偏好为 system 且系统匹配 dark 时，applies 'dark'", () => {
    mockMatchMedia(true);
    setThemePreference('system');
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it("偏好为 system 且系统不匹配 dark 时，applies 'light'", () => {
    mockMatchMedia(false);
    setThemePreference('system');
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('initTheme 之后，系统切换会实时反映（偏好是 system 时）', () => {
    const mm = mockMatchMedia(false);
    localStorage.setItem(THEME_KEY, 'system');
    initTheme();
    expect(document.documentElement.dataset.theme).toBe('light');
    mm.fireSystemChange(true);
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('显式选择 dark 后，系统切换不应影响 data-theme', () => {
    const mm = mockMatchMedia(false);
    localStorage.setItem(THEME_KEY, 'dark');
    initTheme();
    expect(document.documentElement.dataset.theme).toBe('dark');
    mm.fireSystemChange(true);
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('显式选择 light 后，系统切换不应影响 data-theme', () => {
    const mm = mockMatchMedia(true);
    localStorage.setItem(THEME_KEY, 'light');
    initTheme();
    expect(document.documentElement.dataset.theme).toBe('light');
    mm.fireSystemChange(false);
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('切换离开 system 偏好后，旧的系统监听器被移除（listenerCount 归零）', () => {
    const mm = mockMatchMedia(false);
    setThemePreference('system');
    expect(mm.listenerCount()).toBe(1);
    setThemePreference('dark');
    expect(mm.listenerCount()).toBe(0);
  });
});

describe('localStorage 不可用时的降级', () => {
  it('getItem 抛异常时不崩溃，按 system 处理', () => {
    const orig = Storage.prototype.getItem;
    Storage.prototype.getItem = () => { throw new Error('boom'); };
    mockMatchMedia(false);
    expect(() => getThemePreference()).not.toThrow();
    expect(getThemePreference()).toBe('system');
    Storage.prototype.getItem = orig;
  });

  it('setItem 抛异常时不崩溃，仍然应用到 DOM', () => {
    const orig = Storage.prototype.setItem;
    Storage.prototype.setItem = () => { throw new Error('boom'); };
    mockMatchMedia(false);
    expect(() => setThemePreference('light')).not.toThrow();
    expect(document.documentElement.dataset.theme).toBe('light');
    Storage.prototype.setItem = orig;
  });
});

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

beforeEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute('data-theme');
});

afterEach(() => {
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

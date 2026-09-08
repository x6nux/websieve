/**
 * 主题偏好（设计文档「主题切换 + 滚动条覆盖」§2）。
 *
 * 存 localStorage 而非 config.yaml——这是「这台设备、这个人怎么看界面」的
 * 展示偏好，不是代理配置，两者生命周期不同，不该耦合在同一份文件里。
 *
 * 三态：'dark' | 'light' | 'system'。只有 'system' 会订阅系统主题变化；
 * 显式选了 dark/light 之后系统怎么变都不该影响界面，否则「选深色」形同虚设。
 */

export const THEME_KEY = 'websieve:theme';
const VALID = ['dark', 'light', 'system'];

let unsubscribeSystemWatch = null;

function safeGetItem(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function safeSetItem(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // localStorage 不可用时静默降级——展示偏好丢失不该拖累主功能。
  }
}

export function getThemePreference() {
  const v = safeGetItem(THEME_KEY);
  return VALID.includes(v) ? v : 'system';
}

function resolveSystemTheme() {
  return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

function resolve(pref) {
  return pref === 'system' ? resolveSystemTheme() : pref;
}

function applyTheme(pref) {
  document.documentElement.dataset.theme = resolve(pref);
}

function watchSystemIfNeeded(pref) {
  if (unsubscribeSystemWatch) {
    unsubscribeSystemWatch();
    unsubscribeSystemWatch = null;
  }
  if (pref !== 'system') return;

  const mql = window.matchMedia('(prefers-color-scheme: dark)');
  const onChange = () => applyTheme('system');
  mql.addEventListener('change', onChange);
  unsubscribeSystemWatch = () => mql.removeEventListener('change', onChange);
}

export function setThemePreference(pref) {
  const next = VALID.includes(pref) ? pref : 'system';
  safeSetItem(THEME_KEY, next);
  applyTheme(next);
  watchSystemIfNeeded(next);
  return next;
}

/** 应用启动时调用一次：应用当前已保存的偏好，若是 system 则订阅系统切换。 */
export function initTheme() {
  const pref = getThemePreference();
  applyTheme(pref);
  watchSystemIfNeeded(pref);
}

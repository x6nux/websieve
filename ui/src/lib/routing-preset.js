/**
 * 内置分流预设的可选项。`custom` 是「规则」——用户在 config.yaml 里手写
 * 的那份，其余三个是产品内置、完全无视文件 `rules:` 数组的预设。
 *
 * `direct`/`global` 对应路由引擎自己的 `mode` 三态之二
 * （`wsieve_route::Mode::Direct`/`Global`，见 engine.rs 的短路逻辑：
 * 两者从不咨询任何规则）。`china` 对应 `mode: rule` + 内置的
 * `wsieve_route::CHINA_PRESET_RULES`，与 `custom`（同样是 `mode: rule`，
 * 但规则来自文件）的区别只在于规则来源，靠正交的 `rule-preset` 字段区分。
 *
 * `RulesView`（顶部 segmented control）与 `HomeView`（分流模式卡片）
 * 共用同一份 `preset` 状态与 `saveRoutingPreset` 保存函数，也共用这份
 * 选项列表——两处都在描述同一件事，选项列表分叉了就会有一处先改、
 * 另一处忘记跟着改。
 */
export const PRESET_OPTIONS = [
  { value: 'direct', label: '全局直连' },
  { value: 'global', label: '全局代理' },
  { value: 'china', label: '中国大陆' },
  { value: 'custom', label: '规则' },
];

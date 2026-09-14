//! 链路画像的跨进程记忆：把每个服务端的实测 RTT/带宽存下来，下次冷启动
//! 直接落到正确档位，跳过收敛期。
//!
//! 先例是 Linux 的 `tcp_metrics` 与 RFC 2140（TCP Control Block
//! Interdependence）：同一个对端的路径特征在连接之间是复用的，每次都从零
//! 重新学是白白浪费一段传输。
//!
//! **历史只是起点，不是结论。** 换 WiFi、换地点、运营商换出口，路径可能差
//! 一个数量级，此时照着旧记录跳档比没有记录更糟（一条慢链路一上来就开
//! 64 MiB 窗口）。裁决交给第一批实测，见 `LinkProfile::judge_history`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use wsieve_xhttp::link_profile::ProfileSnapshot;

/// 磁盘上的一条记录。
///
/// 不直接给 `ProfileSnapshot` 加 serde derive：那会把 serde 拖进
/// `wsieve-xhttp` 的依赖里，而那一层是纯传输逻辑，没有别的地方需要序列化。
/// 两个字段的转换比一条跨 crate 的依赖便宜。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Entry {
    min_rtt_us: u64,
    delivery_bps: f64,
    /// 写入时刻（Unix 秒）。裁剪用，见 `MAX_AGE` / `MAX_ENTRIES`。
    ///
    /// `serde(default)` 是为了读得动**没有这个字段**的旧文件：那种记录会被
    /// 当成时刻 0，也就是"最旧"，于是在需要裁的时候优先出局。这正是想要的
    /// 行为——旧格式的记录没有年龄信息，宁可让它先走。
    #[serde(default)]
    at: u64,
}

/// 记录的保质期。
///
/// 超过这个年龄就丢掉：路径特征会漂（换了住处、运营商改了出口、服务端搬了
/// 机房），半年前的数字对定档没有参考价值。`judge_history` 那道裁决只在
/// **本次有实测样本之后**才生效，冷启动的第一批 POST 仍然是照着历史档位发
/// 出去的——所以明显过期的记录必须在读进来之前就拦掉。
const MAX_AGE: u64 = 30 * 24 * 3600;

/// 记录条数上限。
///
/// 这份文件此前只增不减：每换一次服务端就多一条，永远不清。单条几十字节、
/// 涨得慢，所以不会"撑爆"什么——但一个无界增长的磁盘文件不该因为"涨得慢"
/// 就不设上限。超了从最旧的开始丢。
const MAX_ENTRIES: usize = 64;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 按 endpoint 索引的画像记录。
#[derive(Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    links: BTreeMap<String, Entry>,
}

/// 画像存储。读写都在内存里，`save` 时一次性落盘。
#[derive(Debug)]
pub struct LinkMetrics {
    path: PathBuf,
    doc: Doc,
}

impl LinkMetrics {
    /// 从磁盘读。文件不存在、读不动、或内容损坏一律当作"没有历史"。
    ///
    /// 损坏不报错是有意的：这是一份**纯优化用途**的缓存，丢了只是回到没有
    /// 自适应记忆的状态。为它中断启动，等于让一个可有可无的文件拥有否决
    /// 整个应用的权力。
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut doc: Doc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let now = now_secs();
        // 过期的在读进来这一刻就丢。放到 `get` 里判也能挡住使用，但那样文件
        // 会一直留着它们——而裁剪的另一半理由正是"别让文件无界增长"。
        doc.links
            .retain(|_, e| now.saturating_sub(e.at) <= MAX_AGE);
        Self { path, doc }
    }

    /// 取某个 endpoint 的历史。
    pub fn get(&self, endpoint: &str) -> Option<ProfileSnapshot> {
        self.doc.links.get(endpoint).map(|e| ProfileSnapshot {
            min_rtt_us: e.min_rtt_us,
            delivery_bps: e.delivery_bps,
        })
    }

    /// 记下某个 endpoint 的最新画像。
    pub fn put(&mut self, endpoint: &str, snap: ProfileSnapshot) {
        self.doc.links.insert(
            endpoint.to_string(),
            Entry {
                min_rtt_us: snap.min_rtt_us,
                delivery_bps: snap.delivery_bps,
                at: now_secs(),
            },
        );
        // 裁在 `put` 里而不是 `save` 里：`save` 只在有变更时调，而条数是被
        // `put` 撑起来的，两者放在一起才不会出现"内存里超了但没人裁"的窗口。
        while self.doc.links.len() > MAX_ENTRIES {
            // 只有刚写的那条不能被淘汰——它是最新的，`at` 最大，天然不会被选中。
            let Some(oldest) = self
                .doc
                .links
                .iter()
                .min_by_key(|(k, e)| (e.at, (*k).clone()))
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.doc.links.remove(&oldest);
        }
    }

    /// 原子落盘。三道保护（临时文件 / fsync / 同目录）的理由全在
    /// `stats::write_atomically` 的注释里，这里直接复用那一份实现——本模块
    /// 曾经自己写了一遍 temp+rename，漏掉了 `sync_all`，也就是漏掉了挡掉电
    /// 的那一道。
    pub fn save(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&self.doc)?;
        crate::stats::write_atomically(&self.path, &text)
    }

    /// 默认存放位置：与 `config.yaml` 同目录。
    pub fn default_path(config_dir: &Path) -> PathBuf {
        config_dir.join("link-metrics.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wsieve-lm-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_saved_profile_comes_back_on_the_next_load() {
        let d = tmpdir();
        let p = LinkMetrics::default_path(&d);
        let mut m = LinkMetrics::load(&p);
        assert_eq!(m.get("https://a.example"), None, "空存储不该凭空变出历史");
        m.put(
            "https://a.example",
            ProfileSnapshot { min_rtt_us: 48_000, delivery_bps: 12.5e6 },
        );
        m.save().unwrap();

        let again = LinkMetrics::load(&p);
        let got = again.get("https://a.example").unwrap();
        assert_eq!(got.min_rtt_us, 48_000);
        assert!((got.delivery_bps - 12.5e6).abs() < 1.0);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 不同 endpoint 各记各的——换服务器就该换一套记录，而不是继承上一台的。
    #[test]
    fn each_endpoint_keeps_its_own_record() {
        let d = tmpdir();
        let p = LinkMetrics::default_path(&d);
        let mut m = LinkMetrics::load(&p);
        m.put("https://a.example", ProfileSnapshot { min_rtt_us: 10_000, delivery_bps: 1e6 });
        m.put("https://b.example", ProfileSnapshot { min_rtt_us: 90_000, delivery_bps: 9e6 });
        m.save().unwrap();
        let again = LinkMetrics::load(&p);
        assert_eq!(again.get("https://a.example").unwrap().min_rtt_us, 10_000);
        assert_eq!(again.get("https://b.example").unwrap().min_rtt_us, 90_000);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 损坏的文件当成"没有历史"，绝不上抛。
    ///
    /// 这是一份纯优化缓存，丢了只是回到没有记忆的状态；让它有权否决启动
    /// 是把代价和重要性搞反了。
    #[test]
    fn a_corrupt_file_degrades_to_no_history_instead_of_failing() {
        let d = tmpdir();
        let p = LinkMetrics::default_path(&d);
        std::fs::write(&p, b"{ this is not json").unwrap();
        let m = LinkMetrics::load(&p);
        assert_eq!(m.get("https://a.example"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 落盘不得留下临时文件——`save` 用 temp+rename，rename 之后 tmp 应当消失。
    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let d = tmpdir();
        let p = LinkMetrics::default_path(&d);
        let mut m = LinkMetrics::load(&p);
        m.put("https://a.example", ProfileSnapshot { min_rtt_us: 1, delivery_bps: 1.0 });
        m.save().unwrap();
        assert!(p.exists());
        assert!(!p.with_extension("json.tmp").exists(), "临时文件没被 rename 掉");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 条数必须有上限，超出时淘汰最旧的那条。
    ///
    /// 这份文件此前只增不减：每换一次服务端就多一条，永远不清。
    #[test]
    fn the_record_count_is_capped_and_evicts_the_oldest() {
        let d = tmpdir();
        let p = LinkMetrics::default_path(&d);
        let mut m = LinkMetrics::load(&p);
        for k in 0..(MAX_ENTRIES + 10) {
            m.put(
                &format!("https://s{k}.example"),
                ProfileSnapshot { min_rtt_us: 1_000 + k as u64, delivery_bps: 1e6 },
            );
            // `at` 是秒级的，同一秒内写完全部记录时它们的 `at` 相同。淘汰只好
            // 退到按 key 排序——这不是"按插入顺序"，所以断言只钉两件事：
            // 条数封住了、最后写进去的那条还在。后者才是功能上非可选的：
            // 淘汰把刚测出来的记录扔掉，等于自适应记忆完全失效。
        }
        assert_eq!(m.doc.links.len(), MAX_ENTRIES, "条数没被封住");
        let last = format!("https://s{}.example", MAX_ENTRIES + 9);
        assert!(m.get(&last).is_some(), "淘汰把刚写入的最新记录扔了");
        m.save().unwrap();
        assert_eq!(LinkMetrics::load(&p).doc.links.len(), MAX_ENTRIES);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 过期记录在**读进来那一刻**就丢，不是等用的时候才判。
    ///
    /// 只在 `get` 里判也能挡住使用，但文件会一直留着它们——而裁剪的另一半
    /// 理由正是别让磁盘文件无界增长。
    #[test]
    fn a_record_past_its_shelf_life_is_dropped_on_load() {
        let d = tmpdir();
        let p = LinkMetrics::default_path(&d);
        let fresh = now_secs();
        let stale = fresh.saturating_sub(MAX_AGE + 1);
        std::fs::write(
            &p,
            format!(
                r#"{{"links":{{
                  "https://old.example":{{"min_rtt_us":9,"delivery_bps":1.0,"at":{stale}}},
                  "https://new.example":{{"min_rtt_us":9,"delivery_bps":1.0,"at":{fresh}}}
                }}}}"#
            ),
        )
        .unwrap();
        let m = LinkMetrics::load(&p);
        assert!(m.get("https://old.example").is_none(), "过期记录还在");
        assert!(m.get("https://new.example").is_some(), "新鲜记录被误删");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 旧格式（没有 `at` 字段）的文件必须读得动。
    ///
    /// 它们会被当成时刻 0 = 最旧，于是一上来就因为超龄被丢掉。这是有意的：
    /// 没有年龄信息的记录不该长期占着位子。关键是**不能读崩**——那会让整个
    /// 文件退化成"没有历史"，连同里面新格式的记录一起。
    #[test]
    fn a_file_from_before_the_timestamp_field_still_parses() {
        let d = tmpdir();
        let p = LinkMetrics::default_path(&d);
        std::fs::write(
            &p,
            br#"{"links":{"https://a.example":{"min_rtt_us":48000,"delivery_bps":12500000.0}}}"#,
        )
        .unwrap();
        // 读得动（不 panic、不整份作废），且那条无年龄记录按"最旧"处理。
        let m = LinkMetrics::load(&p);
        assert!(m.get("https://a.example").is_none(), "无 at 的记录该按最旧丢掉");
        let _ = std::fs::remove_dir_all(&d);
    }
}

//! 规则命中计数的落盘与恢复（设计文档 §11.2）。
//!
//! 为什么要持久化：§11.5 的「命中热度染色」要回答「我这堆规则里哪些是死的」，
//! 而那需要足够长的观察窗口。每次重启清零，热度图就永远是刚开机的样子，
//! 那个 signature 也就失去了意义。
//!
//! 为什么用独立文件而不塞进 config.yaml：config.yaml 是**用户手写的**、
//! 带注释的、含私钥的。往里写机器生成的计数意味着每次退出都要重写一遍那个
//! 文件 —— 阶段 1 为保住注释所做的全部工作会被这一下抵消掉。
//!
//! ## 两条不能让步的失败语义
//!
//! ① **写到一半被杀，不能毁掉已有的文件。** 原地写（`File::create`）先截断
//!    再填内容，截断与 flush 之间被 SIGKILL 就留下 0 字节或半截 JSON ——
//!    用户攒了几个月的热度一次清零。因此走「同目录临时文件 → fsync →
//!    rename」：POSIX 的 rename 是原子的，任一瞬间看到的都是完整的旧文件或
//!    完整的新文件。守它的是 `a_crash_mid_write_leaves_the_previous_file_intact`
//!    —— 那个测试真的起子进程、真的 SIGKILL，不是推理出来的。
//!
//! ② **文件坏了不能拒绝启动。** 命中计数是观察数据，丢了不影响任何功能；
//!    为它挡住代理启动是本末倒置。但也**不能静默**（房规）：一律 `warn!`
//!    出来，守它的是 `a_corrupt_file_is_reported_not_swallowed`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// schema 版本。改变 rule_hits 的键语义时必须 +1 ——
/// 譬如从「规则原文」改成「规则 id」，旧数据会全部错位。
///
/// 对外公开是为了让落盘方引用它而不是写字面量 `1`：字面量在版本号 +1 时
/// 不会报错，只会写出一个自己都读不回来的文件。
pub const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct StatsFile {
    pub version: u32,
    /// 键是**规则原文**（如 "GEOSITE,cn,DIRECT"），不是下标。
    /// 下标会因为用户增删规则而整体错位，恢复出来的热度全是错的。
    ///
    /// 代价是另一头：用户**改写**一条规则等于换了个键，那条规则的历史
    /// 归零，而旧键会作为孤儿一直留在文件里。这是刻意的取舍 —— 归零只是
    /// 少一条观察数据，错位则是把 A 的热度画在 B 上，后者会误导判断。
    ///
    /// ponytail: 孤儿键不清理。上限：每次改规则多留一个键，一年下来是几十
    /// 个几十字节的条目，量级可忽略。升级路径：阶段 5 的规则视图手上有当前
    /// 规则集，落盘前按它过滤一次即可。
    pub rule_hits: HashMap<String, u64>,
}

impl Default for StatsFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            rule_hits: HashMap::new(),
        }
    }
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("stats.json")
}

/// 读。任何失败都退回空统计并**告警**（不静默），绝不阻断启动 ——
/// 命中计数是观察数据，丢了不影响任何功能。
///
/// 崩溃留下的 `stats.json.tmp` 不在这里清：`load` 是只读的，让它带副作用
/// 会让「启动时读一下」变成一个能改动磁盘的动作。残留由下一次 `save`
/// 覆盖（临时文件名是固定的），与 hosts 托管「崩溃残留由下次启动兜底」
/// 的取舍一致。
pub fn load(p: &Path) -> StatsFile {
    let Ok(text) = std::fs::read_to_string(p) else {
        // 文件不存在是首次启动的正常状态，不值得告警
        return StatsFile::default();
    };
    match serde_json::from_str::<StatsFile>(&text) {
        Ok(s) if s.version == VERSION => s,
        Ok(s) => {
            tracing::warn!(
                "{} 的版本 {} 不认识（当前 {VERSION}），命中计数从零开始",
                p.display(),
                s.version
            );
            StatsFile::default()
        }
        Err(e) => {
            tracing::warn!("{} 解析失败（{e}），命中计数从零开始", p.display());
            StatsFile::default()
        }
    }
}

/// 写。先写同目录临时文件、fsync、再 rename。
///
/// 三步缺一不可，各挡一种失败：
///   - **临时文件**挡进程崩溃：原地写会先截断，截断后被杀就只剩空文件。
///   - **fsync** 挡掉电：rename 只保证「目录项的切换是原子的」，不保证新
///     文件的内容已经落盘；不 fsync 的话掉电后可能 rename 生效而内容是空洞。
///   - **同目录** 挡 `EXDEV`：跨文件系统的 rename 根本不是原子操作（多数
///     平台上直接失败）。用 `with_extension` 而非 `temp_dir()` 就是为这个。
///
/// 目录本身不 fsync：那只影响「掉电后新文件是否已可见」，最坏结果是看到
/// 完整的**旧**文件 —— 仍然不是损坏，不值得为一个观察功能付这笔代价。
pub fn save(p: &Path, stats: &StatsFile) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(stats)?;
    write_atomically(p, &text)
}

/// 序列化之后、真正碰磁盘的那一步。
///
/// 单独拆出来不是为了复用，是为了**能被测**：崩溃测试要让子进程把绝大部分
/// 时间花在危险窗口里，而序列化（占单次 `save` 的大头）根本碰不到目标文件。
/// 让子进程循环调用这个函数、复用同一份预先序列化好的文本，「进程被杀时正
/// 处在写文件中途」的概率才接近 1 —— 否则测试会因为没踩中窗口而假绿。
/// 这一点是实证的：拆分之前，把实现换成原地写，崩溃测试照样通过。
fn write_atomically(p: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wsieve-stats-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_survives_restart() {
        let p = path(&tmpdir("rt"));
        let mut s = StatsFile::default();
        s.rule_hits.insert("GEOSITE,cn,DIRECT".into(), 42);
        s.rule_hits.insert("MATCH,日本节点".into(), 7);
        save(&p, &s).unwrap();

        let back = load(&p);
        assert_eq!(back.rule_hits["GEOSITE,cn,DIRECT"], 42);
        assert_eq!(back.rule_hits["MATCH,日本节点"], 7);
    }

    #[test]
    fn missing_file_starts_from_zero() {
        let p = tmpdir("missing").join("nope.json");
        assert!(load(&p).rule_hits.is_empty(), "文件不存在是正常的首次启动");
    }

    #[test]
    fn corrupt_file_does_not_panic() {
        // 上次退出时断电，留下半截 JSON —— 不能因此拒绝启动
        let p = path(&tmpdir("corrupt"));
        std::fs::write(&p, "{ not json at all").unwrap();
        assert!(load(&p).rule_hits.is_empty());
    }

    #[test]
    fn future_version_is_discarded_not_misread() {
        let p = path(&tmpdir("ver"));
        std::fs::write(&p, r#"{"version":99,"rule_hits":{"a":1}}"#).unwrap();
        assert!(
            load(&p).rule_hits.is_empty(),
            "不认识的版本应整体丢弃，而不是按当前 schema 硬读"
        );
    }

    #[test]
    fn no_tmp_file_is_left_behind() {
        let d = tmpdir("atomic");
        save(&path(&d), &StatsFile::default()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件没清干净：{leftovers:?}");
    }

    #[test]
    fn save_creates_missing_directory() {
        // 首次运行时 app_config_dir 可能还不存在
        let d = tmpdir("mkdir").join("nested/deeper");
        let p = path(&d);
        save(&p, &StatsFile::default()).unwrap();
        assert!(p.exists());
    }

    /// 走一遍 main.rs 里那条真实链路：上次退出 → 本次启动 restore →
    /// 运行中 bump → 再次退出 snapshot 落盘。
    ///
    /// 单独测 `save`/`load` 抓不到这里唯一会出错的地方：`restore` 若写成
    /// 累加而非覆盖，每重启一次热度就翻倍。events.rs 那侧有
    /// `rule_hits_restore_replaces_not_adds` 守着覆盖语义，这里守的是
    /// 「两侧接起来之后，跨重启的数字确实是连续的」。
    #[test]
    fn hit_counts_continue_across_a_restart_instead_of_doubling() {
        let p = path(&tmpdir("lifecycle"));

        // 第一次运行：攒下 10 次命中后退出
        let first = crate::events::RuleHits::default();
        for _ in 0..10 {
            first.bump("GEOSITE,cn,DIRECT");
        }
        save(
            &p,
            &StatsFile {
                version: VERSION,
                rule_hits: first.snapshot(),
            },
        )
        .unwrap();

        // 第二次运行：恢复后再命中 3 次
        let second = crate::events::RuleHits::default();
        second.restore(load(&p).rule_hits);
        for _ in 0..3 {
            second.bump("GEOSITE,cn,DIRECT");
        }
        assert_eq!(
            second.snapshot()["GEOSITE,cn,DIRECT"],
            13,
            "恢复后应当接着数（10+3），翻倍说明 restore 变成了累加"
        );

        // 第三次运行：只恢复不命中，数字必须原样不动
        save(
            &p,
            &StatsFile {
                version: VERSION,
                rule_hits: second.snapshot(),
            },
        )
        .unwrap();
        let third = crate::events::RuleHits::default();
        third.restore(load(&p).rule_hits);
        assert_eq!(
            third.snapshot()["GEOSITE,cn,DIRECT"],
            13,
            "空跑一轮不该改变历史"
        );
    }

    /// 键是规则原文的直接后果：用户**改写**一条规则，那条的历史归零，
    /// 且旧键留下来成为孤儿。
    ///
    /// 这不是 bug，是 `rule_hits` 文档注释里写明的取舍 —— 但它是用户能
    /// 观察到的行为，得有测试把它钉住，免得日后有人「顺手」改成按下标存。
    #[test]
    fn editing_a_rule_resets_that_rules_history_and_leaves_an_orphan() {
        let p = path(&tmpdir("edit"));

        let before = crate::events::RuleHits::default();
        for _ in 0..99 {
            before.bump("DOMAIN-SUFFIX,例子.com,节点A");
        }
        before.bump("MATCH,兜底");
        save(
            &p,
            &StatsFile {
                version: VERSION,
                rule_hits: before.snapshot(),
            },
        )
        .unwrap();

        // 用户把出站从「节点A」改成「节点B」—— 对本模块而言就是个新键
        let after = crate::events::RuleHits::default();
        after.restore(load(&p).rule_hits);
        after.bump("DOMAIN-SUFFIX,例子.com,节点B");

        let s = after.snapshot();
        assert_eq!(s["DOMAIN-SUFFIX,例子.com,节点B"], 1, "改写过的规则从头数");
        assert_eq!(
            s["DOMAIN-SUFFIX,例子.com,节点A"], 99,
            "旧键作为孤儿保留（见 rule_hits 的取舍说明），而不是被误算到新键上"
        );
        assert_eq!(s["MATCH,兜底"], 1, "没动过的规则不受牵连");
    }

    // ── 房规：错误不静默 ────────────────────────────────────────

    /// 把 tracing 的输出接到内存里，好断言「告警真的发出来了」。
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl LogCapture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogCapture {
        type Writer = Self;
        fn make_writer(&self) -> Self {
            self.clone()
        }
    }

    /// 抓住 `f` 执行期间本线程发出的日志。用 `with_default`（线程局部）
    /// 而非全局 subscriber：测试是并发跑的，全局的会互相打架。
    fn capture_logs(f: impl FnOnce()) -> String {
        let cap = LogCapture::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, f);
        cap.text()
    }

    /// 房规：错误绝不静默跳过。坏掉的 stats.json 会被吞掉重来，
    /// 但**必须留下痕迹** —— 否则用户看到热度归零时无从查起。
    #[test]
    fn a_corrupt_file_is_reported_not_swallowed() {
        let p = path(&tmpdir("loud-corrupt"));
        std::fs::write(&p, "{ half a jso").unwrap();

        let logs = capture_logs(|| {
            assert!(load(&p).rule_hits.is_empty());
        });
        assert!(
            logs.contains("stats.json") && logs.contains("WARN"),
            "解析失败必须告警且指明是哪个文件，实际日志：{logs:?}"
        );
    }

    /// 版本不认识同样是「悄悄丢数据」，同样要出声。
    #[test]
    fn an_unknown_version_is_reported_not_swallowed() {
        let p = path(&tmpdir("loud-ver"));
        std::fs::write(&p, r#"{"version":99,"rule_hits":{"a":1}}"#).unwrap();

        let logs = capture_logs(|| {
            let _ = load(&p);
        });
        assert!(
            logs.contains("99") && logs.contains("WARN"),
            "版本不匹配必须告警并报出实际版本，实际日志：{logs:?}"
        );
    }

    /// 文件不存在是首次启动的常态，**不**该告警 —— 每次全新安装都刷一条
    /// 警告，会把真正的警告淹没掉。
    #[test]
    fn a_missing_file_is_not_worth_a_warning() {
        let p = tmpdir("quiet-missing").join("nope.json");
        let logs = capture_logs(|| {
            let _ = load(&p);
        });
        assert!(logs.trim().is_empty(), "首次启动不该有噪音，实际日志：{logs:?}");
    }

    // ── 崩溃中途写 ──────────────────────────────────────────────

    /// 子进程入口：不停地写 stats.json，等着被父进程 SIGKILL。
    ///
    /// **循环里只调 `write_atomically`，不调 `save`** —— 序列化 5000 条要
    /// 花掉单次 `save` 的大部分时间，而那段时间根本没碰目标文件。若把它留在
    /// 循环里，子进程被杀时多半正在序列化，测试就永远踩不到危险窗口。
    /// 这不是理论顾虑：实测过，序列化在循环里时，把实现换成原地写测试照样绿。
    ///
    /// 不是普通测试，由 `a_crash_mid_write_leaves_the_previous_file_intact`
    /// 用 `--ignored --exact` 拉起；直接跑（没有那个环境变量）时立刻返回。
    #[test]
    #[ignore = "由父测试以子进程方式拉起，不单独跑"]
    fn crash_mid_write_child() {
        let Ok(dir) = std::env::var("WSIEVE_STATS_CRASH_DIR") else {
            return;
        };
        let p = path(Path::new(&dir));
        // 内容要足够大，好让「截断 → 填充」之间的窗口真的能被踩中。
        // 5000 条 pretty JSON ≈ 250KB，单次写要跨多个 write 系统调用。
        let mut s = StatsFile::default();
        for i in 0..5000u64 {
            s.rule_hits
                .insert(format!("DOMAIN-SUFFIX,例子{i}.com,节点{i}"), i);
        }
        let text = serde_json::to_string_pretty(&s).expect("序列化不该失败");
        // 告诉父进程「可以开杀了」—— 用文件而不是 sleep，免得在慢机器上
        // 父进程杀早了，什么都没测到。
        std::fs::write(Path::new(&dir).join("child-ready"), b"1").unwrap();
        loop {
            write_atomically(&p, &text).expect("子进程里的写不该失败");
        }
    }

    /// **模拟真实崩溃**：起一个子进程反复写 stats.json，在它写的过程中
    /// SIGKILL 掉，然后检查已有的文件还在不在、内容还完不完整。
    ///
    /// 这是本模块最重要的一条。原地写的实现能通过上面所有测试，只会在这里
    /// 挂掉 —— 实证过：把 `save` 换成 `std::fs::write(p, text)` 后本测试
    /// 立刻变红（读到 0 字节 / 半截 JSON）。
    #[test]
    fn a_crash_mid_write_leaves_the_previous_file_intact() {
        let d = tmpdir("crash");
        let p = path(&d);

        // 先落一份「上次运行攒下的历史」。崩溃后必须一字不差地还在。
        let mut prior = StatsFile::default();
        prior.rule_hits.insert("GEOSITE,cn,DIRECT".into(), 123_456);
        prior.rule_hits.insert("MATCH,日本节点".into(), 789);
        save(&p, &prior).unwrap();

        let exe = std::env::current_exe().expect("拿不到测试二进制路径");
        let ready = d.join("child-ready");

        // 杀 12 次，每次落在写循环的不同相位上。单次可能恰好躲开危险窗口，
        // 十几次躲不开 —— 原地写的实现在这个次数下必被抓住。
        for round in 0..12 {
            let _ = std::fs::remove_file(&ready);
            let mut child = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "stats::tests::crash_mid_write_child",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env("WSIEVE_STATS_CRASH_DIR", &d)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("起不来子进程");

            // 等子进程进入写循环（最多 10s，慢机器上也够）
            let t0 = std::time::Instant::now();
            while !ready.exists() && t0.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(ready.exists(), "第 {round} 轮：子进程没进入写循环");

            // 再让它写一小会儿，相位随轮次错开
            std::thread::sleep(std::time::Duration::from_micros(200 + round * 900));
            child.kill().expect("SIGKILL 失败");
            let status = child.wait().expect("回收子进程失败");
            assert!(!status.success(), "第 {round} 轮：子进程应当是被杀死的");

            // 关键断言：此刻磁盘上的 stats.json 必须是**某一个完整版本**。
            // 要么还是崩溃前那份历史，要么已经是子进程写完的那份；
            // 绝不能是空文件或半截 JSON。
            let raw = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("第 {round} 轮：stats.json 没了：{e}"));
            let parsed: StatsFile = serde_json::from_str(&raw).unwrap_or_else(|e| {
                panic!(
                    "第 {round} 轮：崩溃后 stats.json 不是完整 JSON（{} 字节）：{e}",
                    raw.len()
                )
            });
            assert!(
                parsed.rule_hits.contains_key("GEOSITE,cn,DIRECT")
                    || parsed.rule_hits.len() == 5000,
                "第 {round} 轮：读到的既不是旧历史也不是新快照，共 {} 条",
                parsed.rule_hits.len()
            );
        }
    }
}

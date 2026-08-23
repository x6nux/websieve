//! 服务端库（Task 13 起逐步充实；本模块先提供 mux 协商纯函数）。

use wsieve_proto::hello::MuxId;

/// spec §7.4：按客户端偏好顺序取第一个服务端也支持的；无交集 → yamux + fallback=true。
/// 永不失败——任何情况下连接都要建起来。
pub fn pick_mux(client_prefs: &[MuxId], server_enabled: &[MuxId]) -> (MuxId, bool) {
    for &c in client_prefs {
        if server_enabled.contains(&c) {
            return (c, false);
        }
    }
    (MuxId::Yamux, true) // 基线回退
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 交集命中时取客户端偏好顺序的第一个，而非服务端顺序。
    #[test]
    fn intersection_takes_first_client_pref() {
        let (chosen, fb) = pick_mux(
            &[MuxId::H2mux, MuxId::Smux],
            &[MuxId::Smux, MuxId::Yamux, MuxId::H2mux],
        );
        assert_eq!(chosen, MuxId::H2mux);
        assert!(!fb);
    }

    /// 无交集 → yamux + fallback。
    #[test]
    fn no_intersection_falls_back_to_yamux() {
        let (chosen, fb) = pick_mux(&[MuxId::Picomux], &[MuxId::Smux, MuxId::H2mux]);
        assert_eq!(chosen, MuxId::Yamux);
        assert!(fb);
    }

    /// 客户端偏好为空 → 直接回退。
    #[test]
    fn empty_client_prefs_fall_back() {
        let (chosen, fb) = pick_mux(&[], &[MuxId::Smux]);
        assert_eq!(chosen, MuxId::Yamux);
        assert!(fb);
    }

    /// 重复/乱序输入行为正常：命中即返回，服务端列表含重复无影响。
    #[test]
    fn dedup_and_odd_inputs_sane() {
        let (chosen, fb) = pick_mux(
            &[MuxId::Smux, MuxId::Smux, MuxId::Yamux],
            &[MuxId::Yamux, MuxId::Yamux, MuxId::Smux],
        );
        assert_eq!(chosen, MuxId::Smux);
        assert!(!fb);

        // 服务端全空也算无交集
        let (chosen, fb) = pick_mux(&[MuxId::Yamux], &[]);
        assert_eq!(chosen, MuxId::Yamux);
        assert!(fb); // 注意：即使客户端要的就是 yamux，服务端未启用也算 fallback
    }
}

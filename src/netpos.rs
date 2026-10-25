//! 网络位置检测 —— 判断本机在校园网阶梯的哪一级
//!
//! 连接支持矩阵（产品定案）：
//! - 骨干网 ↔ 任何：✅ 任意方向（本机 CampusDirect 适合「创建聊天」）
//! - 宿舍路由后 ↔ 骨干网：✅ 方向固定（本机 BehindRouter 适合「加入聊天」，靠出站自由拨出）
//! - 宿舍 ↔ 宿舍：v1 不支持（未来打洞/中继才解决）
//!
//! 纯本地判断：不发包、不查 DNS，只枚举网卡 IPv4。
//! 虚拟网卡（utun/awdl/vmnet 等）会产生假 10.x 地址，必须排除——
//! 这是实测踩过的坑：Mac 上的代理/虚拟化软件常挂在 10.x 段。

use local_ip_address::list_afinet_netifas;
use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq)]
pub enum Position {
    /// 骨干网直连（有真实 10.x 地址），含该地址
    CampusDirect(String),
    /// 个人路由器后（仅 192.168/172 私网）——可拨出，不可被连
    BehindRouter,
    /// 没有可用网络
    Offline,
}

impl Position {
    /// 状态栏一句话（十岁孩子能懂，并把"创建/加入"的推荐直接说清）
    pub fn show_hint(&self) -> String {
        match self {
            Position::CampusDirect(ip) =>
                format!("校园骨干网在线（{ip}）：别人可以直接连你，适合【创建聊天】"),
            Position::BehindRouter =>
                "你在路由器后面：别人连不到你，但你可以主动连别人——适合【加入聊天】，请让对方创建".into(),
            Position::Offline =>
                "没检测到校园网：请先连上网络再试".into(),
        }
    }

    /// UI 引导用：当前是否适合当「创建方」（监听）
    pub fn prefer_listen(&self) -> bool {
        matches!(self, Position::CampusDirect(_))
    }
}

/// 网卡名是否为虚拟/隧道（启发式：前缀匹配）
fn is_virtual(name: &str) -> bool {
    let n = name.to_lowercase();
    const PREFIXES: [&str; 13] = [
        "utun", "tun", "tap", "veth", "awdl", "llw", "bridge", "ap", "docker",
        "vmnet", "vmen", "gif", "stf",
    ];
    PREFIXES.iter().any(|p| n.starts_with(p)) || n.contains("vmware") || n.contains("virtual")
}

pub fn detect() -> Position {
    let ifaces = match list_afinet_netifas() {
        Ok(v) => v,
        Err(_) => return Position::Offline,
    };
    let mut campus: Option<String> = None;
    let mut priv_net = false;
    for (name, ip) in ifaces {
        let IpAddr::V4(v4) = ip else { continue };
        if v4.is_loopback() || is_virtual(&name) {
            continue;
        }
        let o = v4.octets();
        if o[0] == 10 {
            // 校园网骨干地址（10.0.0.0/8）。多个时取第一个（物理网卡通常排前）
            campus.get_or_insert_with(|| v4.to_string());
        } else if v4.is_private() {
            priv_net = true;
        }
    }
    if let Some(ip) = campus {
        Position::CampusDirect(ip)
    } else if priv_net {
        Position::BehindRouter
    } else {
        Position::Offline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detect_never_panics_and_returns_valid() {
        let p = detect();
        // 任何环境下都应是合法枚举，且提示语非空
        assert!(!p.show_hint().is_empty());
        match &p {
            Position::CampusDirect(ip) => assert!(ip.starts_with("10."), "骨干地址必须是10.x: {ip}"),
            _ => {}
        }
    }
}

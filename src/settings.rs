// SPDX-License-Identifier: AGPL-3.0-or-later
//! User-editable values, separate from account identity and live worker state.
use crate::config::Config;
use anyhow::{bail, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub idle_cores: usize,
    pub active_cores: usize,
    pub max_slots: usize,
    pub placement: String,
    pub numa_nodes: Vec<u32>,
    pub cpu_offset: usize,
    pub core_type: String,
    pub priority: String,
    pub memory_limit_mb: u64,
    pub native_wait_hours: u64,
    pub idle_after_secs: u64,
    pub pause_on_battery: bool,
    pub gpu_enabled: bool,
    pub compute_memory_limit_mb:u64,
    pub hidden_sidebar: Vec<String>,
}
impl Settings {
    pub fn from_config(c: &Config) -> Self {
        Self {
            idle_cores: c.idle_cores,
            active_cores: c.active_cores,
            max_slots: c.max_slots,
            placement: c.placement.clone(),
            numa_nodes: c.numa_nodes.clone(),
            cpu_offset: c.cpu_offset,
            core_type: c.core_type.clone(),
            priority: c.priority.clone(),
            memory_limit_mb: c.memory_limit_mb,
            native_wait_hours: c.native_wait_hours,
            idle_after_secs: c.idle_after_secs,
            pause_on_battery: c.pause_on_battery,
            gpu_enabled: c.gpu_enabled,
            compute_memory_limit_mb:c.compute_memory_limit_mb,
            hidden_sidebar:c.hidden_sidebar.clone(),
        }
    }
    pub fn write(&self, c: &mut Config) {
        c.idle_cores = self.idle_cores;
        c.active_cores = self.active_cores;
        c.max_slots = self.max_slots;
        c.placement = self.placement.clone();
        c.numa_nodes = self.numa_nodes.clone();
        c.cpu_offset = self.cpu_offset;
        c.core_type = self.core_type.clone();
        c.priority = self.priority.clone();
        c.memory_limit_mb = self.memory_limit_mb;
        c.native_wait_hours = self.native_wait_hours;
        c.idle_after_secs = self.idle_after_secs;
        c.pause_on_battery = self.pause_on_battery;
        c.gpu_enabled = self.gpu_enabled;
        c.compute_memory_limit_mb=self.compute_memory_limit_mb;
        c.hidden_sidebar=self.hidden_sidebar.clone();
    }
    pub fn topology_changed(&self, old: &Self) -> bool {
        self.placement != old.placement
            || self.numa_nodes != old.numa_nodes
            || self.cpu_offset != old.cpu_offset
            || self.core_type != old.core_type
            || self.compute_memory_limit_mb!=old.compute_memory_limit_mb
    }
    pub fn apply_value(&mut self, row: usize, value: &str, max: usize) -> Result<()> {
        let value = value.trim();
        let integer = || {
            value
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("请输入非负整数"))
        };
        match row {
            0 => {
                let n = integer()?;
                if n == 0 || n > max as u64 {
                    bail!("空闲核心应在 1..={max}");
                }
                self.idle_cores = n as usize;
            }
            1 => {
                let n = integer()?;
                if n > max as u64 {
                    bail!("使用时核心应在 0..={max}");
                }
                self.active_cores = n as usize;
            }
            2 => {
                let n = integer()?;
                if n > 64 {
                    bail!("并发上限应在 0..=64，0 为自动");
                }
                self.max_slots = n as usize;
            }
            3 => {
                let v = if value.is_empty() { "auto" } else { value };
                if !["auto", "system", "numa", "cache"].contains(&v) {
                    bail!("请选择 auto/system/numa/cache");
                }
                self.placement = v.into();
            }
            4 => {
                let mut nodes = Vec::new();
                for n in value.split(',').filter(|s| !s.trim().is_empty()) {
                    nodes.push(
                        n.trim().parse::<u32>().map_err(|_| {
                            anyhow::anyhow!("节点用逗号分隔，例如 0,1；留空选择全部")
                        })?,
                    );
                }
                nodes.sort_unstable();
                nodes.dedup();
                self.numa_nodes = nodes;
            }
            5 => {
                self.cpu_offset =
                    usize::try_from(integer()?).map_err(|_| anyhow::anyhow!("偏移过大"))?
            }
            6 => {
                if !["auto", "p", "e", "windows"].contains(&value) {
                    bail!("请选择 auto/p/e/windows");
                }
                self.core_type = value.into();
            }
            7 => {
                if !["idle", "below_normal", "normal"].contains(&value) {
                    bail!("请选择 idle/below_normal/normal");
                }
                if cfg!(unix) && value != self.priority {
                    bail!("此平台优先级需退出后编辑 priority，并从正常优先级的终端重新启动");
                }
                self.priority = value.into();
            }
            8 => self.memory_limit_mb = integer()?,
            9 => {
                let n = integer()?;
                if n > 720 {
                    bail!("等待时间应在 0..=720 小时，服务器接受期限优先");
                }
                self.native_wait_hours = n;
            }
            10 => {
                let n = integer()?;
                if n == 0 {
                    bail!("空闲判定至少 1 秒");
                }
                self.idle_after_secs = n;
            }
            11 => {
                self.pause_on_battery = match value {
                    "true" => true,
                    "false" => false,
                    _ => bail!("请选择 true/false"),
                }
            }
            12 => self.gpu_enabled = match value {"true"=>true,"false"=>false,_=>bail!("请选择 true/false")},
            13 => {let n=integer()?;if !(4096..=65536).contains(&n){bail!("CPU+GPU 内存上限应在 4096..=65536 MiB");}self.compute_memory_limit_mb=n;},
            _ => bail!("未知设置"),
        }
        Ok(())
    }
    pub fn rows(&self) -> Vec<(&'static str, String, &'static str)> {
        vec![
            (
                "离开时核心",
                self.idle_cores.to_string(),
                "即时生效；不超过本机可用逻辑处理器。",
            ),
            (
                "使用时核心",
                self.active_cores.to_string(),
                "0 表示使用电脑时暂停计算。",
            ),
            (
                "并发上限",
                self.max_slots.to_string(),
                "0 自动；控制同时计算的身份/任务组数。",
            ),
            (
                "调度 placement",
                self.placement.clone(),
                "auto 自动 / system 系统 / numa 节点 / cache 缓存；空值按 auto。",
            ),
            (
                "NUMA 节点",
                self.numa_nodes
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                "留空为全部；按拓扑填写，例如 0 或 0,1。",
            ),
            (
                "CPU 顺序偏移",
                self.cpu_offset.to_string(),
                "对可用 CPU 顺序旋转；多实例需自行确认预算不重叠。",
            ),
            (
                "核心类型",
                self.core_type.clone(),
                "auto 自动 / p 性能优先 / e 能效优先 / windows 系统。",
            ),
            (
                "进程优先级",
                self.priority.clone(),
                "idle 空闲 / below_normal 低于正常 / normal 正常。",
            ),
            (
                "内存软线 MiB",
                self.memory_limit_mb.to_string(),
                "默认 2048；0 关闭。持续超线后排空任务、有序换进程。",
            ),
            (
                "等待原生库 小时",
                self.native_wait_hours.to_string(),
                "默认 48；0 立即更新。接受期到期/服务器拒绝会提前更新。",
            ),
            (
                "空闲判定 秒",
                self.idle_after_secs.to_string(),
                "没有键鼠输入多久后按离开时预算运行。",
            ),
            (
                "电池时暂停",
                self.pause_on_battery.to_string(),
                "true 暂停 / false 继续。",
            ),
            ("NVIDIA GPU 预览",self.gpu_enabled.to_string(),"true 自动探测榜单收益；学习适合 GPU 的场景，失败或过慢暂缓并回 CPU。"),
            ("CPU+GPU上限 MiB",self.compute_memory_limit_mb.to_string(),"Windows 进程合计提交硬上限，默认 8192；显存另计。改动后排空并重启。"),
            ("侧栏显示项目",format!("隐藏 {} 项",self.hidden_sidebar.len()),"Enter 打开勾选菜单；也可在面板按 D，显示改动立即保存，无需重启。"),
        ]
    }
    pub fn choices(row: usize) -> &'static [&'static str] {
        match row {
            3 => &["auto", "system", "numa", "cache"],
            6 => &["auto", "p", "e", "windows"],
            7 => &["idle", "below_normal", "normal"],
            11 | 12 => &["true", "false"],
            _ => &[],
        }
    }
}

/// Stable keys persisted in TOML; labels can change without losing preferences.
pub const SIDEBAR_ITEMS: &[(&str, &str)] = &[
    ("contribution", "贡献（整组）"), ("session", "  本次贡献 / 任务数"),
    ("speed", "  总速度 / CPU、GPU 速度"), ("pending", "  待提交"),
    ("today", "  今日贡献"), ("account", "  账号 / 本机积分"),
    ("confirmation", "  确认进度"), ("resources", "资源（整组）"),
    ("cpu", "  系统 CPU 占用"), ("process_cpu", "  本程序 CPU 占用"),
    ("gpu", "  GPU 状态 / 占用 / 最近错误"), ("vram", "  显存"),
    ("memory_limit", "  CPU+GPU 提交上限"), ("memory", "  系统 / 程序内存"),
    ("utilisation", "  计算线程利用率"), ("budget", "  核心预算"),
    ("concurrency", "  并发 / 积压"), ("effective_cores", "  实得核心 / 大小核"),
    ("core_map", "  核心状态色块"), ("site", "全站在线 / 待算 / 线路"),
    ("ranking", "本周贡献榜"),
];
impl Settings {
    pub fn sidebar_visible(&self, key: &str) -> bool {
        !self.hidden_sidebar.iter().any(|k| k == key)
    }
    pub fn toggle_sidebar(&mut self, index: usize) {
        if let Some((key, _)) = SIDEBAR_ITEMS.get(index) {
            if self.sidebar_visible(key) { self.hidden_sidebar.push((*key).into()); }
            else { self.hidden_sidebar.retain(|k| k != key); }
        }
    }
}

pub struct Channel {
    pub desired: Settings,
    pub message: String,
    pub topology: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validate_settings_before_mutating_identity() {
        let mut c = Config::fresh(6);
        let id = c.verifier.clone();
        let mut s = Settings::from_config(&c);
        assert!(s.apply_value(0, "0", 16).is_err());
        s.apply_value(3, "", 16).unwrap();
        assert_eq!(s.placement, "auto");
        s.apply_value(4, "1,0,1", 16).unwrap();
        assert_eq!(s.numa_nodes, vec![0, 1]);
        assert!(s.apply_value(9, "-1", 16).is_err());
        s.apply_value(9, "48", 16).unwrap();
        s.apply_value(12,"true",16).unwrap();
        assert!(s.apply_value(13,"2048",16).is_err());s.apply_value(13,"8192",16).unwrap();
        assert!(s.apply_value(12,"maybe",16).is_err());
        s.write(&mut c);
        assert_eq!(c.verifier, id);
        assert_eq!(c.native_wait_hours, 48);
        assert!(c.gpu_enabled);
        assert_eq!(c.compute_memory_limit_mb,8192);
    }
}

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
    }
    pub fn topology_changed(&self, old: &Self) -> bool {
        self.placement != old.placement
            || self.numa_nodes != old.numa_nodes
            || self.cpu_offset != old.cpu_offset
            || self.core_type != old.core_type
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
        ]
    }
    pub fn choices(row: usize) -> &'static [&'static str] {
        match row {
            3 => &["auto", "system", "numa", "cache"],
            6 => &["auto", "p", "e", "windows"],
            7 => &["idle", "below_normal", "normal"],
            11 => &["true", "false"],
            _ => &[],
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
        s.write(&mut c);
        assert_eq!(c.verifier, id);
        assert_eq!(c.native_wait_hours, 48);
    }
}

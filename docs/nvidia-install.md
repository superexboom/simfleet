# NVIDIA 测试包安装

preview.14 增加 `sfg3`：更细的私有页和 16 位索引，在相同预算下运行更大批量。sfg3 必须配合 preview.14 或更新客户端；不要把其清单或内核覆盖进 sfg1/2 目录。解压后应形成 `engines/gpu/<引擎ID>-sfg3-sm120/` 等独立目录。客户端优先选择同引擎、同架构的 sfg3，再回落 sfg2/sfg1。本次套件为 `dac6ecbaf97e` / sm120，其他架构需要对应套件。性能与验证边界见 [私有页优化](gpu-private-pages.md)。

本地吞吐测试版 preview.12 可使用独立的 `sfg2` 套件，并兼容下面列出的 `sfg1`。优先选择同引擎、同架构的 `sfg2`；探测上限为 12 秒，完整批次仍为 30 秒。实测范围和本地版变动见 [GPU 吞吐优化](gpu-throughput.md)。

客户端：[v0.9.0-preview.14](https://github.com/superexboom/simfleet/releases/tag/v0.9.0-preview.14) 或更新版本。CPU 原生引擎：[engine-dac6ecbaf97e](https://github.com/superexboom/simfleet/releases/tag/engine-dac6ecbaf97e)，Windows 为 `dac6ecbaf97e-sfe2-v3.dll`，使用 PGO＋ThinLTO。GPU 引擎：[gpu-dac6ecbaf97e-sfg3](https://github.com/superexboom/simfleet/releases/tag/gpu-dac6ecbaf97e-sfg3)。三个 Release 分开下载，不把引擎二进制塞进客户端压缩包。

1. 解压 Windows 客户端包到副机独立目录。不要直接复制主机的 `simfleet.toml`、计算身份或任务日志；第一次启动按正常流程配置副机身份、账号和核心数。
2. 按显卡架构及引擎 ID 选择套件。本轮提供 RTX 50 系列的 `sm120`，解压到 `simfleet.exe` 同目录，得到 `engines/gpu/dac6ecbaf97e-sfg3-sm120/`。包内只有三个二进制、清单、LICENSE 和 NOTICE。其他架构的旧引擎包不能用于当前引擎。
3. 使用可用的 NVIDIA 显卡驱动；无需另装 CUDA Toolkit、Python 或编译器。包使用 CUDA 12.9 工具链生成，需要支持该 cubin 的驱动；太旧的驱动可能加载失败。
4. 启动客户端，等同版 PGO 原生引擎自动下载并通过参考校验。在 `S → NVIDIA GPU 预览` 开启。默认 `S → CPU+GPU上限 MiB` 为 8192；这是系统提交内存，不含显存。客户端还会根据显卡可用显存限制批量。
5. 看“GPU状态”“程序GPU”和“贡献 → 其中GPU”。`探测计算` 只学习适用性，完成正常任务后才增加 GPU 贡献。等待、暂缓或没有同版 GPU 包时由 CPU 继续计算。

preview.9 修复了“系统可用内存不足 8 GiB 就完全阻止 GPU”的旧门槛。现在按配置总限额、可用物理内存和显存共同缩批；物理内存预留为总量的 1/16，限制在 512 MiB–2 GiB。16 GiB 主机可在仍有 7 GiB 或约 4.8 GiB 可用时尝试小批量，4096 MiB 配置仍是 CPU＋GPU 合计提交硬上限。较大任务可能因实际预算不足回 CPU，调低总限额不会增加系统可用内存。

| GPU 计算能力 | 常见 GeForce 系列 | 下载文件后缀 | 验证状态 |
|---|---|---|---|
| 7.5 / Turing | GTX 16、RTX 20 | `sm75-windows-x64.zip` | 本轮未提供当前引擎套件 |
| 8.9 / Ada | RTX 40 | `sm89-windows-x64.zip` | 本轮未提供当前引擎套件 |
| 12.0 / Blackwell | RTX 50 | `sm120-windows-x64.zip` | 已在该架构设备上做完整结果对照，未逐型号覆盖 |

选择依据是驱动报告的 CUDA 计算能力，表中的系列是常见对应关系，套件没有单型号锁定。当前未发布 RTX 30 系列的 sm86 包。

此前发布的 sm75 和 sm89 套件清单明确记录 `device_validated: false`；桥接层通过 CPU 参考验证不等于目标 GPU 已完成实机验收，也不保证各场景有吞吐收益。每个新场景仍做原始 WASM 分片校验，完整任务仍做 canary；不一致会停用 GPU，临时资源故障会回 CPU 并恢复。

GPU 架构和上游引擎都必须匹配。程序会自动从已安装的包中选择本机架构，但目前不自动下载 GPU 包；上游引擎变化后需要对应版本套件。不要改目录名、清单架构或引擎号尝试让旧包冒充新包。

副机出现问题时，保留目录中的 `dispatch.log`、`gpu-diagnostics/last-failure.json` 和 `gpu-diagnostics/previous-failure.json` 即可追查。诊断不包含登录凭证；不要公开 `simfleet.toml` 或任务身份文件。关闭 GPU 开关可继续原生 CPU 计算，保留套件目录不会强制启用。

贡献统计的口径：CPU 和 GPU 分别按实际工作时间计算速度，等待任务和暂停时停止计时；探测和失败工作仍计入 GPU 活跃时间。总速度按两者活跃时间的并集计算，设备速度不能直接相加。账号榜单旁的速度是最近约半小时入账增量，可能包含多台设备并滞后于本机完成时间。

GPU 子进程限制随配置总限额调整；父 Job 负责 CPU＋GPU 合计硬上限。设置 65536 MiB 不等于给每局模拟分配 64 GiB，也不会取消超时：当前探测为 12 秒，sfg1/2 完整批次为 30 秒，sfg3 按实际模拟数量在 30–120 秒内增长。状态 120 表示单局私有页不足，后续会提高该场景配额并缩批；状态 2 表示输出或地址范围限制。资源回退详情保存在 `gpu-diagnostics/last-resource-fallback.json`，超时详情仍在 `last-failure.json`。

preview.11 将 CUDA 驱动内存分配失败单独分类：完整批次失败后降低 GPU 批量上限并重建，保留可用场景；GPU 退出等待在后台进行，CPU 继续计算。`CUDA driver error 2` 不能与单局输出回退码 2 混同。诊断会记录失败 API 和分配信息；“上次错”是历史记录，当前状态需看 GPU 状态行。

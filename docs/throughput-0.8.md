# simfleet 0.8 性能与拓扑调度

0.8.1 已继续减少紫卡串行 lead 的重复处理。最终固定请求为 25.68 s → 2.47 s，13 个混合任务为 34.58 s → 14.66 s；实现、完整报文对照及测试限制见 [紫卡 lead 优化](riven-lead-0.8.1.md)。下文保留 0.8.0 阶段的测试条件与结果。

## 实现范围

保持同一任务输入、相同 seed/index、同一战斗循环、相同 gas/work 与合并顺序。在客户端和本地编译的原生引擎中减少重复准备、JSON 往返、线程创建与跨节点迁移；不需要改 WFSim 的服务器或公开派发协议。原生库附带补丁和源码指纹，仍必须通过网站 WASM 对照。

- **每行一次准备。** 不再每局重建同一份战斗参数和报告用 Forma 成本。参数在首次执行的计算线程创建，随任务结束释放。
- **原生分片留在 DLL。** 客户端持有不透明句柄，任务与 DLL 生命周期通过 `Arc` 关联。每批仅返回进度整数，最终返回一次分数；原来的多 KB/局 JSON 不再跨 DLL/调度线程往返。
- **原顺序合并。** 乱序完成的分片暂存，按运行索引递增合并；不能把浮点求和换成树形并行归约。数字先做无分配检查，JS 表示有歧义的整数、负零等仍走原规范化路径。
- **单 worker 直接执行。** 紫卡评分不再每批创建一个线程、等待和销毁；固定请求的解析计划在每条线程保留最近一个，请求改变或新搜索时替换。
- **修复重复永久分配。** 武器 class/slot/召唤来源直接引用静态数据，mod pool 列表按武器保留一次；紫卡分类路径也不再每次泄漏字符串。
- **有界尾部切片。** 快速原生任务尽量避免一局一调度，最小切片受实测单局成本限制，目标不超过约 20 ms，不强迫慢武器形成长尾。
- **线程退出后卸载。** 引擎切换和取消后，后台 reaper 等线程退出及其 TLS 析构完成，再释放旧 DLL，调度线程不等待长调用。
- **切换原生后释放旧 WASM。** 快速接口也执行原有 30 秒回收规则，避免绕过普通入口后把之前创建的 WASM 实例一直保留。

DLL 缺少扩展、启动逐位校验失败或 `SIMFLEET_NATIVE_FAST=0` 时沿用普通 API。旧原生库和 WASM 仍可工作。

## 同条件测速

Ryzen 7 9700X，Windows x64，同一引擎 `872e90ffaf0d`、同一数据包 `dd8982bee216`、原生 x86-64-v3。编译优化等级仍沿用原设置。基线是上一轮的 0.7.4，candidate 为 0.8 实现；每组交替运行旧/新、新/旧，三次取中位数，不用单次最好值。

| 固定任务 | 线程 | 基线 | 优化后 | 吞吐比 |
|---|---:|---:|---:|---:|
| Braton Prime / demolisher / cycle | 1 | 3.35 s | 2.82 s | 1.19× |
| Braton Prime / demolisher / cycle | 6 | 0.79 s | 0.66 s | 1.20× |
| Braton Prime / demolisher / cycle | 16 | 0.39 s | 0.32 s | 1.22× |
| Fulmin Prime / heavy gunner / semi-auto | 1 | 0.84 s | 0.47 s | 1.79× |
| Fulmin Prime / heavy gunner / semi-auto | 6 | 0.20 s | 0.12 s | 1.67× |

Fulmin 一组使用同一个 0.8 客户端、同一个 DLL，只在基线设置 `SIMFLEET_NATIVE_FAST=0`，隔离出本地快速 ABI 的收益。所有轮次的分数、metric、work 一致；完整 counts 另由 `check-native` 对照 WASM。实际增幅随每局模拟成本改变，复杂多人场景在战斗内部花费更多，不能把最快案例外推到整个站点。

紫卡固定请求的初测为 28.26 s → 25.25 s，完整结果、103 步、20,880 场、work `9065743294` 相同。反向对照仍得到相同结果；受后台编译影响的一轮为 37.46 s → 32.90 s，不作为独立的稳定性能数字。主要减少的是评分端的解析和线程等待，lead 仍需按原算法回放搜索步骤，不能宣称搜索已完全并行。

最终二进制的混合回归同时投入 12 行榜单和 1 个紫卡搜索，16 线程、cache 调度。普通接口 35.29 s，快速接口 36.47 s，13 份完整答案一致；这组单轮结果未显示总完成时间收益，最后结束的是紫卡的串行 lead。较快的榜单阶段不等于所有混合任务的最后完成时间都会缩短，应按实际任务比例评估吞吐。

## NUMA、处理器组与 X3D

`placement=auto` 检测到多 NUMA 节点、多末级缓存或 Intel 大小核时启用；普通单节点单缓存 CPU 保持系统调度。`placement=system` 完全交给系统；`numa` 和 `cache` 可显式比较。

Windows 分别读取实际 NUMA 节点、处理器组、CPU set、物理核心和 LLC 容量。线程绑定使用组内掩码并设置 CPU set，统计也使用组号与核心编号组合，避免把两组的 CPU 0 当成同一个核心。Linux 读取 sysfs 拓扑并尊重进程允许的 CPU 集。macOS 保持现有 QoS 路径。

平台依据：[Windows CPU Set 结构](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-system_cpu_set_information)、[处理器组](https://learn.microsoft.com/en-us/windows/win32/procthread/processor-groups)、[NUMA 支持](https://learn.microsoft.com/en-us/windows/win32/procthread/numa-support)。CPU set 的核心/缓存索引是组内索引，不能直接当成全系统唯一编号。

分配顺序为物理核心优先，再 SMT；多 NUMA 节点轮流分配预算，任务首先使用起始节点/缓存域的线程，其他域空闲时可接活。AMD 不再被拓扑代码整体跳过；如果两个 CCD 的缓存大小不同，同节点内先用缓存更大的 CCD 的物理核心，再另一 CCD 的物理核心，最后 SMT。缓存大并不保证每种战斗都更快，因此保留系统调度对照。

想让 Xeon 的多个客户端按节点分开，先运行：

```powershell
.\simfleet.exe cpu
```

根据实际输出分别配置，例如：

```toml
placement = "numa"
numa_nodes = [0]
idle_cores = 10
active_cores = 10
```

另一进程使用 `[1]`。不能假定节点号等于插槽号；SNC、固件配置和处理器组都会影响节点分布。在同一允许节点集合中按 10 线程分三份，可使用 `cpu_offset=0/10/20`，但必须确保预算覆盖范围不重叠。它是固定划分，不是跨进程的共享调度器。

```powershell
.\simfleet.exe bench --lanes 10 --repeat 5 --backend native --placement system
.\simfleet.exe bench --lanes 10 --repeat 5 --backend native --placement numa
.\simfleet.exe bench --lanes 10 --repeat 5 --backend native --placement cache
```

本机仅一节点、一个 32 MiB LLC。已实测拓扑识别与逐线程落点，并以模拟拓扑覆盖双节点 128 线程、大小缓存双 CCD、SMT 顺序和显式节点划分。**尚未在 Xeon Platinum 或 X3D 实机证明吞吐收益，也没有使用强制 NUMA 内存绑定或大页。** 首次分配尽量本地化并不能让整个进程的共享只读数据都复制到本地。

同一最终二进制和 DLL、同样 6 线程，在本机交替比较系统调度与 `placement=cache`：每种三轮、每轮重复三次，轮内先取中位数再跨轮取中位数。系统为 **0.655718 s**，cache 为 **0.521640 s**，吞吐比 **1.257×**。这里的收益来自优先使用不同物理核心与减少线程迁移，不是本机多了 X3D 缓存；不能与不同测试时段的引擎增幅直接相乘。当前机器因此选择 cache，核心预算仍为 6。

## 内存与验证

当前检查结果：27 个客户端单元测试、4 个原生接口/计划缓存测试通过；另外在本机显式运行了缓存识别与线程落点检查、取消后行句柄及 DLL 生命周期检查。4 把武器的 32 行榜单和 9 条 gas 向量全部与参考 WASM 一致，比较完整 counts、分数位模式、metric、work，紫卡搜索的完整结果也一致。缓存绑定下的本地协议测试完成 3 次维护、36 行任务，保存故障恢复后正常继续。Clippy 完成，仅保留项目已有的 11 个风格告警。

未修补的原生库在 20 万次短模拟后约 86 MiB 私有提交；修补后热身约 66 MiB，20 万次结束约 65 MiB，线程退出后约 63 MiB。2,000 次创建、并发计算和销毁原生行上下文，热身后维持约 40–41 MiB。Python 宿主计入这些数值，不等于生产客户端绝对占用。

进程级旧库和永久静态缓存仍可能累积，保留 0.7.4 的内存软线维护。它是额外兜底，不能代替按任务释放资源。

复现工具：`tools/compare-throughput.py`、`tools/bench-riven.py`、`tools/probe-native-memory.py`、`tools/check-maintenance.py`。原始结果在 `.bench-throughput/`，打包时提供验证记录，不含真实账号凭据。

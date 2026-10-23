# simfleet 两轮性能与内存优化

面向同类分布式计算客户端的实现、验证与迁移指南

日期：2026-10-21。适用版本：simfleet 0.8.0 / 0.8.1，以及发布阶段的原生库修复。本文按“客户端与引擎数据路径”和“紫卡串行 lead”划分两轮，补充此前的内存治理。读者无需了解本次开发对话。

**主要结论：CPU 满载不等于有效吞吐已到上限。** 这两轮收益主要来自删除重复准备、跨边界序列化、元数据重建和被丢弃的报告，而不是增加线程数、降低模拟精度或修改战斗公式。兄弟项目最值得先检查的是对象生命周期、每次计算的准备成本，以及串行协调阶段反复执行的纯函数。

本文明确区分已实现的优化、实测结果与待验证方向。源码为 AGPL-3.0-or-later；移植具体代码和补丁时应保留原项目署名与许可。思想和架构可以按目标项目重新实现。

## 1. 系统模型与不可破坏的约束

simfleet 是 WFSim 志愿计算任务的独立客户端。服务器分发任务，客户端选择原生动态库或 WASM 执行，返回可由其他机器重复验证的结果。大部分优化不要求修改服务器或公共派发协议；原生引擎通过本地源码补丁扩展能力。

| 术语 | 工程含义 | 可迁移到的场景 |
|---|---|---|
| 榜单行 / row | 同一请求执行 N 次独立模拟，按顺序汇总 | Monte Carlo、参数扫描、批量推理 |
| shard | 一次或一组运行的统计分片 | 分块计算结果、局部累加器 |
| lead | 搜索协调步骤：推进起点、生成待评分候选、整理答案 | 优化器主线程、调度器、迭代求解控制阶段 |
| score | 对 lead 派发的候选进行模拟评分 | 可并行的目标函数评估 |
| lane | 线程池中的计算线程，不等于永久独占一个物理核心 | 固定 worker、线程内执行上下文 |
| gas / work | 按引擎约定计算的操作量与贡献量 | 可验证工作量，不是 CPU 利用率或墙钟时间 |

优化前先冻结以下约束：相同输入和引擎数据、相同随机种子及运行索引、相同候选访问顺序与平局规则、相同模拟次数、相同浮点合并顺序，以及完整 score / counts / gas / work。计时和调度可以改变，答案及协议语义不能跟着机器改变。

本项目保留原战斗循环、计量源和比较器。若兄弟项目允许统计误差或近似求解，可以另设精度契约；不能把近似算法与本报告的等价优化混作同一基准。

## 2. 收益、基线与测量边界

本机是 Ryzen 7 9700X，8 个物理核心、16 个逻辑线程、单 NUMA 节点、一个 32 MiB L3。引擎 ID 为 `872e90ffaf0d`，数据包为 `dd8982bee216`。主测试使用 Windows x64 / AVX2 原生库。该机器不是 X3D，也不是多插槽 Xeon。

| 对照项目 | 同条件结果 | 可以得出的结论 |
|---|---|---|
| 第一轮 Braton，6 线程，0.7.4 → 0.8.0 | 0.79 → 0.66 s，约 1.20× | 减少重复准备与数据搬运有收益 |
| 第一轮 Braton，16 线程 | 0.39 → 0.32 s，约 1.22× | 在本机增加线程后收益仍存在 |
| Fulmin，6 线程，同一客户端和库，普通 API → 快速 ABI | 0.20 → 0.12 s，约 1.67× | 单局越轻，边界开销占比可能越高 |
| 同一构建，6 线程，system → cache | 0.655718 → 0.521640 s，约 1.257× | 本机物理核心优先和减少迁移有收益 |
| 第二轮固定紫卡请求，总耗时 | 25.683789 → 2.471992 s，约 10.39× | 此请求原先主要被 lead 的重复处理限制 |
| 同一紫卡请求，lead 累计 | 24.226002 → 1.223506 s | 串行路径本身可以大幅减负 |
| 同一紫卡请求，最后一次 lead 调用 | 0.521992 → 0.032415 s | 搬走收尾已不是这个样例的主要机会 |
| 最终 12 行榜单 + 1 个紫卡，16 线程 | 34.582094 → 14.660670 s，约 2.36× | 混合任务的总完成时间也得到改善 |

第一轮榜单对照为三轮交替运行取中位数；部分早期脚本从两位小数的日志提取时间，不能将其当作微秒级精密测量。Fulmin 使用相同 EXE / DLL，仅关闭原生快速 ABI，隔离这条数据路径的收益。拓扑对照每种模式三轮、每轮重复三次，先取轮内中位数再取跨轮中位数。

第二轮先做三组交替对照，获得约 10.36×；修正多起点暂停标志边界后，再将最终 DLL 连续测三次，中位数为 2.472 s。正式发布 DLL 从远端重新下载后复测为 2.499 s，完整报文和结果一致。最终混合任务是单轮旧/新对照，新版另一次为 14.794 s，不能据此声称所有业务负载稳定快 2.36 倍。

**不能把表内不同阶段的倍率直接相乘。** 第一轮混合测试曾得到 35.29 → 36.47 s，没有总时长收益，因为最后一个紫卡的串行 lead 仍是瓶颈。只有拆开阶段耗时，才看得到下一步应该优化哪里。

固定紫卡请求的 103 步、20,880 场候选战斗、最终结果和总 work `9065743294` 均一致。这里的 fights 是评分端计数；总 work 还包含最终复算。完整请求在随附的 `evidence/riven-request.json`。

## 3. 第一轮：从生命周期修复长期内存增长

### 3.1 把内存分成四类

不要只根据任务管理器曲线判断“缓存”或“泄漏”。先按归属区分：请求对象、线程局部状态、进程级静态缓存、分配器或动态库卸载后的保留。Windows 同时记录私有提交量与工作集；工作集下降可能只是页面被换出，不能证明分配已释放。

| 确认的问题 | 改动 | 迁移时需要证明 |
|---|---|---|
| 每次准备武器时复制并 `Box::leak` class / slot / summoned_by | 借用原始静态数据中的字符串 | 这些字段不会被组装参数、变体或请求修改 |
| 每次复制 mod pool 列表并永久保留 | 按有限的武器 ID 保留一次引用切片 | key 空间来自固定数据集，不能来自任意用户输入 |
| 紫卡分类后备路径重复泄漏字符串 | 返回已有静态字符串的借用 | 原容器生命周期确实覆盖使用期 |
| 新快速入口绕过旧 WASM 清理路径 | 快速调用也执行 30 秒闲置回收 | 所有后端切换入口都遵守同一生命周期规则 |

mod pool 修复仍保留一次进程生命周期分配，不能宣称完全零泄漏。它把“随请求次数增长”变成“随固定武器数量增长”。自定义名称、用户构筑或无限变体不能照搬这种永久缓存。

原型观测中，20 万次短模拟后旧库私有提交约 86 MiB；修补后热身约 66 MiB，结束约 65 MiB，退出线程后约 63 MiB。2,000 次原生 row 创建、并发计算和销毁，热身后约 40-41 MiB。它们含 Python 宿主开销，不是生产客户端的绝对内存需求。

### 3.2 动态库必须活到 TLS 析构结束

“最后一次函数调用已返回”不足以证明动态库可以卸载。线程退出时，Rust TLS 的析构代码仍可能位于该 DLL 中。提前卸载会让线程跳到已解除映射的代码。

实现上，原生 row 和任务持有 `Arc<Code>`，线程池另外保留它使用过的引擎引用及退休线程的 `JoinHandle`。取消任务或切换池时关闭派发通道，把等待线程退出交给后台 reaper；所有线程完成退出与 TLS 清理后才释放引擎引用。reaper 创建失败时走同步等待，保持生命周期正确。

```text
stop assigning jobs
  -> finish outstanding calls
  -> worker exits, TLS destructors run
  -> join(worker)
  -> release library references
  -> unload DLL
```

本实现会将同一池曾使用过的代码引用保留到池销毁，因此没有声称每次后端切换都能立刻释放全部旧库。吞吐、取消响应与内存回收需要一起衡量。

### 3.3 有序更换进程是兜底

默认软线为 2,048 MiB，持续 60 秒超线进入排空；达到两倍软线则立即排空。停止领取新任务，继续已有任务和续租，提交或保存结果后替换进程。排空最长 15 分钟，超时通过正常关闭路径交还未完成任务。

保存失败必须保留 dirty 标记，后续循环重试，成功前不因维护丢掉待交结果。用户主动暂停或退出不应被维护逻辑“自动复活”。测试覆盖守护模式、无守护模式、强制超线和保存失败恢复。

软线不是硬上限，排空期间仍可能超线。该机制不能替代源头修复，也不能证明用户此前 13400 运行一两天达到 8 GB 的所有原因已经定位；当时没有对应进程转储。兄弟项目需要自己的长时间部署数据。

## 4. 第一轮：一次准备，分片留在引擎内部

### 4.1 从“每局准备”改为“每任务准备”

旧路径将一次完整模拟 API 当作最小单元，同一请求会反复解析、解析配装和准备战斗参数。若一个任务跑 N 次，成本近似为：

```text
old = N * (prepare + fight + JSON) + merge + report
new = prepare + N * fight + ordered_merge + one_report
```

本地补丁暴露不可变 `PreparedBoard`。row 创建时只保存请求；第一次实际计算在已经完成线程放置的 worker 上，通过 `OnceLock<Result<PreparedBoard, ...>>` 初始化。后续运行按原 seed/index 调用原引擎函数。

这要求准备结果只读、可安全共享；随机数状态、每局临时量和可变累加器不能误放进共享准备对象。初始化失败也被记住，避免每个 worker 反复重试同一个无效请求。

### 4.2 用不透明句柄减少 JSON 往返

普通接口每局返回数 KB shard JSON，宿主解析后再交回引擎合并。新增本地 ABI 保留任务状态和分片，只让进度整数、最终分数跨边界。

| 接口 | 职责与约束 |
|---|---|
| `sfe_board_abi()` | 返回 ABI 版本，宿主显式探测 |
| `sfe_board_new(body, len)` | 创建 row 句柄；运行次数必须有效 |
| `sfe_board_runs(row, from, count)` | 认领不重叠区间；锁外模拟；返回连续已合并数，失败返回负值 |
| `sfe_board_score(row, out_len)` | 仅对全部完成且未失败的 row 评分 |
| `sfe_free(ptr, len)` | 在分配输出的库内释放输出 |
| `sfe_board_drop(row)` | 所有使用它的调用结束后销毁 row |

ABI 不暴露 Rust `Vec`、`String`、引用布局或分配器所有权。宿主持有的是指针和函数表，原生内部保留完整类型。一个 DLL 分配的对象必须由同一个 DLL 的释放入口回收。

```text
dispatcher -> range jobs -> worker pool
                         -> shared immutable preparation
                         -> per-run original fight
                         -> ordered accumulator inside DLL
dispatcher <- progress   <- final score once
```

“分片不出 DLL”不等于所有 JSON 都消失：异常数值仍走原兼容路径，最终 score 调用仍构造报告输入。优化集中在高频的每局往返，而不是宣传零拷贝或无分配。

### 4.3 顺序合并与背压

计算顺序可乱，累加顺序必须固定。实现以 `BTreeMap<from, Vec<Shard>>` 暂存完成区间，只有 `from == next` 才取出，逐局调用原 `merge`。互斥锁保护认领、完成队列和累加器，战斗计算在锁外进行。

```text
ready[from] = pieces
while ready contains next:
    for piece in ready.remove(next):
        accumulator.merge(piece)
        next += 1
```

重叠区间、越界、panic 或计算失败会使整行失败；不完整累加器不能评分。取消时也不能释放仍被后台任务使用的句柄。

当前状态量受任务 runs 和未合并区间约束，runs 上限为 1,000,000；这不是固定字节数的内存上限。极慢的前段可能导致大量后段暂存。面向更大的任务，建议另加按字节计的在途窗口或背压；这项不属于当前已经实现的优化。

## 5. 保留数值与搜索语义

### 5.1 不随意改变浮点归约

浮点加法不满足结合律。树形归约、按完成顺序合并、把多局先合成一个大 shard，都可能与原有逐局左折叠得到不同末位。想要逐位复现，必须固定原始运行索引和累加顺序，而不只是使用同一随机种子。

### 5.2 去掉 JSON 之前先审查它改变了什么

WFSim 的参考路径经过 JavaScript Number。Rust 的整数、负零和非有限数值不一定能原样穿过这个边界。仅仅拿原生类型直接 merge，可能悄悄改变结果。

本实现使用一个不生成文本的 serde 检查器遍历 shard：整数绝对值超过 2^53、负零、非有限浮点等情况进入原 JSON / JS 规范化路径；普通值保留类型。累加器同样检查。另一条文本优化只跳过有限非整数的重复格式化，保留整数舍入和整数读取器需要的表示形式。

迁移时应测试 `2^53` 附近整数、`u64` 大值、`-0`、指数形式、非有限值、字符串中的数字，以及 JSON 库是否启用了正确回读浮点的设置。不要把只在本项目成立的“安全直传条件”套到另一种编码协议。

### 5.3 线程数有时也是算法参数

上游优化器的批宽是 `max(threads * 4, 16)`。WASM 固定宽度 16，因此原生入口仍给请求写入 `threads: 1`，保留搜索行为。随后仅对“一个 worker”的 `par_map` 特判为当前线程直接执行，去掉创建线程、等待和销毁。

这是删除线程包装成本，并非将评分串行化：外层客户端已经把候选切给多个 lane。兄弟项目应检查嵌套线程池，不能既让外层吃满核心，又让每个内层任务建立全核线程池。

冻结的解析计划按线程保留最近一个请求形状，去掉 `quick_fleet` 消息体后比较其余请求。形状变化或 `fresh=true` 时替换；每轮消息只是为计划附上本轮 fleet 数据。这减少重复解析，但仍有计划克隆，不能视为完全无复制。

## 6. 第一轮：线程预算、尾部切片与拓扑

最终 `/api/board/score` 可能生成完整报告并重放代表性战斗，应作为共享池中的任务，而不是在派发线程同步执行。网络同步、续租、交结果沿用后台执行。控制线程的职责是维护状态和派发，不应被长时间格式化、计算或阻塞等待占住。

普通切片目标约 250 ms。原生快速路径在尾部设置最小切片：根据实测单局成本计算，最多 8 局，目标不超过约 20 ms，最后不超过剩余局数。它防止轻任务退化成“一局一派发”；对于很慢的单局仍允许单局，20 ms 不是抢占期限或最坏响应保证。

拓扑策略的实现要点如下：

| 维度 | 当前策略 | 不能推断成什么 |
|---|---|---|
| 物理核心 / SMT | 按层级先占物理核心，再用兄弟线程 | SMT 必定慢或应该禁用 |
| NUMA | 不同节点轮流分配预算，任务先找本域 worker，空闲时跨域取活 | 已实现跨节点共享队列的独立分片或内存强制绑定 |
| 多 LLC / X3D | 同一节点、同一优先层级内先选择较大缓存 | 已在 X3D 验证增益或大缓存对所有任务都更快 |
| Windows | 区分 CPU set ID、处理器组、组内编号、实际 NUMA、L3 容量 | 一个 64 位掩码能覆盖整台大服务器 |
| Linux | 从 sysfs 读拓扑，并尊重允许的 CPU 集 | 所有容器、cpuset 或极大 CPU 数配置都已实机覆盖 |
| 多实例 | `numa_nodes` 与 `cpu_offset` 显式划分 | 跨进程自动协调、自动防止重叠 |

`auto` 在多节点、多缓存域或 Intel 大小核上启用；普通单节点单缓存保持系统调度。`system` 可作对照。macOS 保留已有 QoS 引导，不声称可以像 Windows/Linux 一样绑定指定核心。

使用 Windows 拓扑时，缓存和核心索引需要带上处理器组；NUMA 节点不能简单等同于 CPU 插槽。首次准备尽量放到计算线程上，有利于首次触碰的局部性，但代码没有强制 NUMA 内存策略，也没有把全部全局静态数据复制到每个节点。

```toml
# 每个实例按本机 cpu 命令的输出选择节点。
placement = "numa"
numa_nodes = [0]
idle_cores = 10
active_cores = 10
cpu_offset = 0
```

多个实例必须使用各自的计算身份。`cpu_offset` 是允许 CPU 列表的旋转偏移，不是“物理核心编号”；预算和偏移仍需人工确认不重叠。本次没有多插槽 Xeon 或 X3D 实机吞吐数据，只有本机落点测试及模拟拓扑回归。

## 7. 第二轮：降低串行 lead 的实际工作量

### 7.1 把 lead 与最终验算分开计时

quick-fleet 的交互是 `lead -> 多 worker score -> lead`。浏览器 worker 无共享搜索栈，上游通过回放已有路径、查询已得分缓存推进搜索。即使没有重新打同一场战斗，候选处理、key 构造、偏好资料查询和答案整理仍可能反复执行。

样例总耗时约 25.68 s，其中 lead 累计 24.23 s；最后一次调用只有约 0.52 s。仅把收尾转移到空闲核心，无法解决前面各轮反复付出的成本。

### 7.2 延迟生成中间报告

旧路径在某些起点仍等待评分时，仍执行答案身份计算、池排序、候选 materialize 和摘要准备；外层看到 pending 后只返回待评任务，前面的报告工作被丢弃。

修改后，每个起点仍按原顺序运行、收集它的待评分候选和进度。只要本轮还有等待，就不做随后无用的答案合并和最终报告。单独实施这一项时，样例为 26.86 → 24.43 s，是有收益的删减，但还不是主要提速来源。

暂停判定必须表示“本轮仍存在任何待算工作”。只看最后一次 `score()` 的 `missed` 会漏掉前面起点的暂停；反过来，上一轮残留的标志也可能误伤本轮未调用评分的非法起点。实现增加 `has_pending_work()`，由本轮 full / rough 待算队列判断，并对跨起点暂停做累计。

```text
for each start in original order:
    descend(start)
    paused |= has_pending_work_in_this_call()
    record_progress()
    if paused: skip answer materialization

if paused: return progress and pending work
else:      build the final report
```

这段伪代码强调控制流，不能替代补丁里的完整语义。关键回归包括“前起点暂停，后起点缓存命中”和“上轮标志为真，本轮首起点非法”。

### 7.3 缓存稳定元数据，不缓存会变化的判断

`QBuild` 是本请求表内的索引组合。lead 回放时，同一组合的 key 和 preference 不变，因此增加两个线程局部表：`QBuild -> String` 与 `QBuild -> Preference`。`Preference` 保留原有 drain、卡名、赋能名和进化次序，实际比较仍调用上游 `preferred`。

每表最多 4,096 项；满时清空再填充，新搜索 `fresh` 时整体重置，leader 线程结束时销毁。这是简单有界 memoization，不是 LRU，也不是零分配：命中仍克隆字符串或小型元数据结构，但省去了更贵的重建与查表。

缓存与 `FLEET` 使用不同的 TLS `RefCell`。评分读取可能已经持有 `FLEET` 的借用，若 key 查询再递归借同一容器，就会出现运行时借用冲突。换语言实现时，对应的是避免重入持锁和隐藏死锁。

| 缓存设计问题 | 本实现的选择 | 迁移检查 |
|---|---|---|
| key 的有效范围 | 一个冻结请求 / 一代搜索 | 武器、模组表、紫卡 roll、数据版本改变后，数字索引不能继续复用 |
| 失效 | `fresh` 重置，换请求替换解析计划 | 将 epoch 或完整上下文纳入缓存归属 |
| 预算 | 每表 4,096 项 | 项数上限不是字节上限；记录实际 value 大小和命中率 |
| 对象 | key 与稳定 preference | 最佳元素顺序、评分、spread 或依赖评分的 identity 不可盲目缓存 |
| 释放 | 任务换代与线程退出 | 不把缓存挂到进程级静态容器里无限保留 |

加入该缓存后，固定请求总耗时降至约 2.47 s，lead 累计约 1.22 s。没有改变 103 轮交互和评分任务，说明本次收益来自少做重复工作。这里不能推出缓存更多内容会更快：过大的工作集可能反过来挤掉热数据。

## 8. 正确性和长期运行的验证方法

先建立结果契约，再计时。对这类可重复确认的分布式计算，至少需要以下层次：

| 验证层 | 本项目执行内容 | 它防止的失误 |
|---|---|---|
| 单元边界 | 乱序/重叠/不完整分片；数值边界；多起点暂停；计划换代 | 只测试顺序执行的正常路径 |
| 完整参考答案 | 32 行真实榜单 + 9 条 gas 向量；score 位、metric、全 counts、work | 最终分数碰巧接近，内部计量已变 |
| 搜索 transcript | 每轮 pending/progress、每批 score 响应、最终完整结果 | 搜索路径改变但最后恰好找到同一答案 |
| 生命周期 | 取消中任务、线程退休、引擎切换、旧 WASM 回收 | 提前卸载 DLL 或后台仍引用旧句柄 |
| 保存与重启 | 3 次维护、36 行任务；写盘故障后恢复 | 为了回收内存丢结果或停止重试 |
| 持续内存 | 20 万短调用、2,000 row 生命周期、40 次换请求 | 仅观察第一次任务结束 |
| 发布实物 | 三平台构建，真实动态加载，下载后重算 | 编译成功但目标机器不能加载 |

第二轮 6 种请求覆盖多起点、预筛选开/关、预算截断、Torid 自定义紫卡、同 ID 改 roll、切回原武器。40 次复用同一 leader 和评分池的结果一致，私有提交第 10/20/30/40 次约 144.79 / 144.95 / 143.04 / 144.35 MiB，线程退出后约 141.54 MiB。这个进程同时加载两份库和 Python，绝对值不宜与生产 EXE 比较。

新增回归应在原实现上先失败，再在修复后通过。`wfsim-truth` 的限定范围穷举用于交叉核对搜索行为；保持原启发式算法不等于证明找到所有场景的全局最优解。

性能测量固定输入、seed、run count、引擎、数据、DLL hash、核心预算和优先级。避免后台编译争抢 CPU，分开冷启动与热运行，AB/BA 交替，保留原始样本并报告中位数。worker 累计 CPU 时间可相加，但它不是整个任务的墙钟时间；优化目标是有效任务数/秒或相同 work/秒。

## 9. 发布工程：优化代码必须真正被使用

### 9.1 上游引擎 ID 与本地构建修订分开

旧发布流程看到 `engine-ID` 已有同名 DLL 就跳过；客户端看到缓存文件也继续使用旧库。结果可能是源码已经优化，用户仍在跑旧版本。

本地实现用 `native-engine/revision` 版本化，发布为 `872e90ffaf0d-sfe1[-v3].dll`，Linux/macOS 使用各自扩展名。客户端先尝试当前修订，再走旧库和现有回退。引擎 ID 标识要复现的上游语义，本地修订号和文件 hash 标识具体实现。后续替换既有引擎的行为应提高修订，不覆盖旧附件。

`.build.json` 记录上游提交、wrapper 提交、CPU 目标、库 hash、已应用补丁及逐文件 hash。打包时核对附带源码与清单，发布后核对真实附件，不能仅相信构建日志。补丁不再适用时需要显式检查实际启用的特性，不能假定工作流“成功”就包含所有优化。

### 9.2 Linux 动态库的 TLS 模式

远端 Linux 测试曾出现 `cannot allocate memory in static TLS block`。默认 mimalloc 构建使用 initial-exec TLS，和运行时 `dlopen` 的库组合时会受 glibc 静态 TLS 预留空间限制。该库启用 `local_dynamic_tls` 后重新构建，实际加载及两种 CPU 构建的 WASM 对照通过。

```toml
[target.'cfg(target_os = "linux")'.dependencies.mimalloc]
version = "0.1"
default-features = false
features = ["v2", "local_dynamic_tls"]
```

只修改 Linux 动态库的分配器构建，EXE 与 Windows/macOS 保持原设置。迁移时应在真实加载器中验证；不要用 `LD_PRELOAD` 或放大 TLS 预留暂时掩盖发布缺陷。

## 10. 给兄弟项目的实施顺序

| 顺序 | 实施内容 | 进入下一步前的验收 |
|---|---|---|
| 1 | 固定一组轻任务、重任务、迭代搜索和混合负载 | 建好完整结果对照与阶段计时 |
| 2 | 审计循环中的永久分配、全局缓存、线程与 DLL 生命周期 | 多轮同输入/换输入测试不持续增长 |
| 3 | 将只读准备对象提到任务级，保留原计算函数 | 顺序、逆序、并发分片输出一致 |
| 4 | 将大量内部结果留在引擎，缩小 ABI 搬运 | 所有数值边界和取消路径通过 |
| 5 | 去掉嵌套单 worker 线程、重复解析和中间报告 | 搜索 transcript 一致 |
| 6 | 给重复纯计算加任务级有界缓存 | 换请求/换数据不会命中旧语义，内存有界 |
| 7 | 调整尾部切片，实测核心/缓存/NUMA 放置 | 同预算 AB 对照且没有长尾倒退 |
| 8 | 建立构建修订、来源清单和真实加载测试 | 新用户与缓存旧库的用户都加载预期产物 |

轻任务优先看 prepare、序列化、分配与调度；重任务才进一步看内核热点、数据布局和 SIMD。缓存和拓扑策略的依据是实际工作集与瓶颈，不是 CPU 品牌或“开更多线程”本身。

不建议直接复制本项目的 4,096 项、20 ms、250 ms 或 2 GiB 等常量。它们是本实现的边界与取舍，不是通用最佳值。移植时先沿用保守默认值，再根据目标机器的命中率、分配量、队列延迟、内存和吞吐调参。

本次没有采用的尝试也值得保留：将客户端从 `opt-level="s"` 改为 3、对原生库开启 fat LTO，均未测到可靠收益；不把“编译优化等级更高”当作性能结论。

## 11. 可以继续做，但本次没有实现

**可恢复搜索状态机。** 保存各起点当前位置、fill/sweep 游标、已访问集合和缓存，避免每轮从头回放。若原协议执行 K 轮且历史路径逐步变长，重复前缀成本可能随 K 显著累积；具体复杂度取决于缓存和控制流。新状态机必须保留多起点访问顺序、预算、去重、平局和进度语义，不能直接并行所有起点后合并一个“近似相同”的答案。

**最终候选复算并行。** 候选之间独立，可用本地扩展接口返回完整待验算集合，交现有 worker 池，按原索引收集、稳定排序、累计全部候选 work 后截断。不能只复算最后留下的 N 个，因为被截断候选也可能贡献规定工作量。这个样例最后一次 lead 调用只有约 32 ms，但高 final-runs 请求可能仍值得拆。

**更大规模 NUMA 与背压。** 本地队列、节点内存分配/副本、按缓存工作集安排任务，以及按字节限制乱序暂存，都需要目标服务器实测。当前没有强制 NUMA 内存绑定、大页、X3D 在线自适应调度或跨进程协商。

**把 lead 搬到空闲核。** 当前 lead 已在计算线程运行，其他核可接别的任务。TLS 绑在线程上，不能把下一轮调用任意迁移到另一线程。改变亲和性只改变运行地点，不能删除串行依赖；若为搬移而频繁丢失缓存，反而可能更慢。

## 12. 复现入口与代码索引

移植包提供 Markdown、PDF、六个上游补丁、基准请求、精简测量记录和 Python 检查工具。补丁对应冻结的 WFSim 快照，不能不检查差异就应用到另一个版本。工具依赖完整 simfleet checkout 和实际 EXE/DLL；包本身不是可独立编译的引擎，也不含账号数据。

### 12.1 本项目的最小复现

在完整仓库中准备两个独立目录放基线和候选 EXE/DLL，保留各自 hash。`bench` 不领取生产任务，但可能读取网站发布清单、下载引擎或自检。真正离线对照使用保存的 WASM、数据包及榜单。

```powershell
# 对照同一客户端/DLL的普通接口与快速ABI；不代表关闭lead优化。
python tools/compare-throughput.py `
  --baseline candidate/simfleet.exe --candidate candidate/simfleet.exe `
  --candidate-reference --order evidence/board-order.json `
  --lanes 6 --repeats 3 --placement cache --out result-board

# 完整紫卡搜索，在两个独立进程分别执行；之后比较全部结果与round摘要。
python tools/bench-riven.py baseline/engine.dll `
  evidence/riven-request.json --lanes 6 --out baseline-riven.json
python tools/bench-riven.py candidate/engine.dll `
  evidence/riven-request.json --lanes 6 --out candidate-riven.json

# 换请求、换紫卡roll和连续fresh搜索；Windows私有提交测量。
python experiments/check-lead-regression.py `
  baseline/engine.dll candidate/engine.dll --out result-regression --cycles 40

# 短调用及row生命周期内存探针，完全在本地运行。
python tools/probe-native-memory.py candidate/engine.dll --mode runs `
  --loops 200000 --lanes 6
python tools/probe-native-memory.py candidate/engine.dll --mode contexts `
  --loops 2000 --lanes 6
```

路径中的 `baseline/engine.dll` 和 `candidate/engine.dll` 是测试目录约定，可换成实际库路径。内存维护工具 `tools/check-maintenance.py` 另建本地协议服务器，需要 `psutil`；脚本参数中显式提供 EXE、缓存、引擎/数据 ID、任务和输出目录。

### 12.2 固定源码链接

核心性能实现固定在提交 `5e440c0`；Linux TLS 修复在 `5a78921`；发布来源保护在 `ef9d4ae`。链接使用完整提交，不随未来 main 变化。

| 阅读目标 | 文件与入口 |
|---|---|
| 任务准备、句柄 ABI、认领和顺序合并 | [R1 原生 row 实现](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/native-engine/src/board.rs)：`Row::run`、`sfe_board_*` |
| 数值直传判定与回退 | [R2 数值检查](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/native-engine/src/exact.rs)：`carry`、`carry_acc`；[R3 JS 规范化](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/json_numbers.rs) |
| 冻结请求与单线程入口 | [R4 原生 wrapper](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/native-engine/src/lib.rs)：`PLAN`、`optimize_plan`、`sfe_optimize` |
| 六个可审查上游补丁 | [R5 补丁目录](https://github.com/superexboom/simfleet/tree/5e440c064d5bf39fd6a5cef581cef0fe58751266/native-engine/patches)：001 准备；002 单worker；003 元数据泄漏；004 计划；005 报告；006 lead缓存 |
| worker / DLL 生命周期 | [R6 宿主引擎](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/engine.rs)：`NativeBoard`、`Exec::run`、`Pool::drop` |
| 切片、自检与本地构建优先级 | [R7 工作管理](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/work.rs)：`BoardRun::job`、`native_board_canary`、`riven_canary` |
| 任务派发与leader线程退休 | [R8 Fleet](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/fleet.rs)：`dispatch`、`fill_lanes`、`let_go` |
| 拓扑与核心选择 | [R9 placement](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/placement.rs)、[R10 affinity](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/affinity.rs) |
| 内存软线与结果保存 | [R11 维护状态机](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/maintenance.rs)、[R12 journal](https://github.com/superexboom/simfleet/blob/5e440c064d5bf39fd6a5cef581cef0fe58751266/src/sync.rs) |
| Linux动态库 TLS | [R13 目标平台依赖](https://github.com/superexboom/simfleet/blob/5a789214ba44a88bbcbf6f27ade71641b832a0ae/native-engine/Cargo.toml) |
| 发布与来源校验 | [R14 原生工作流](https://github.com/superexboom/simfleet/blob/ef9d4aef76c961fd7f8c36ef82045a07c3d54289/.github/workflows/native-engine.yml)、[R15 打包](https://github.com/superexboom/simfleet/blob/ef9d4aef76c961fd7f8c36ef82045a07c3d54289/tools/package.ps1) |

对应上游参考为 [WFSim 8a00c91](https://github.com/magenie33/wfsim/tree/8a00c91ed79f53c2f56dd20d7aca0d6fe1b187c0)。本地 Windows 发布库使用此快照；远端构建也用过同一引擎 ID 的后续源码快照，具体以每份 `.build.json` 为准，不能只从引擎 ID 推断二进制来源。

公开验证记录：[客户端三平台发布](https://github.com/superexboom/simfleet/actions/runs/39571385253)、[原生构建与 Linux 修复验证](https://github.com/superexboom/simfleet/actions/runs/39572617272)、[主分支三平台 CI](https://github.com/superexboom/simfleet/actions/runs/39574138308)。交付版本：[v0.8.1](https://github.com/superexboom/simfleet/releases/tag/v0.8.1)；原生构建：[engine-872e90ffaf0d](https://github.com/superexboom/simfleet/releases/tag/engine-872e90ffaf0d)。

# 榜单吞吐与 NVIDIA GPU 验证

PGO + ThinLTO 原生引擎已完成部署。后续 NVIDIA 工作已推进到完整战斗与多任务批量后端，并完成 CPU＋GPU 吞吐对照；它尚未接入主客户端的实网派单。最新结果见 [NVIDIA 完整榜单后端](gpu-board-backend.md)。下文保留第一阶段的 PGO 和 GPU 原语实验条件与数据。

## 固定条件与结果

Windows x64，Ryzen 7 9700X，RTX 5080，驱动 616.92；Rust 1.99.0 / LLVM 23.1.1。引擎 `99dec9ad6d01`，上游提交 `3747fab2571809ae1920bf1f20afaf87e1b6ee0b`，数据 `75f4884878f5`，参考 WASM `ae4c0012e9c3`。基线和 PGO 都是同一份源码、同样六个 simfleet 补丁及 x86-64-v3 目标。

PGO 使用 15 类真实榜单，各 128 次模拟、8 条线程，预热和计时共两遍作为训练。另一组 11 把武器只用于验证，没有参与训练。`tools/bench-board.py` 直接使用客户端的原生批次接口及常驻线程池，计入每行准备、模拟、按序合并和最终评分，比较完整 JSON 答案。每组交替执行，预热后测三遍取中位数。测量期间不并行编译。

| 负载 | 线程 | 基线总耗时 | PGO 总耗时 | 吞吐比 |
|---|---:|---:|---:|---:|
| 15 类训练形状，各 128 run | 1 | 17.257 s | 11.948 s | 1.444× |
| 同上 | 8 | 2.394 s | 1.671 s | 1.433× |
| 同上 | 16 | 1.546 s | 1.106 s | 1.398× |
| 11 把未训练武器，各 128 run | 8 | 5.048 s | 4.170 s | 1.211× |
| 同上 | 16 | 3.113 s | 2.621 s | 1.188× |
| 4 条完整榜单，各 1,000 run | 8 | 1.217 s | 0.849 s | 1.434× |
| 同上 | 16 | 0.796 s | 0.567 s | 1.404× |

完整行是 Braton Prime / demolisher / cycle、Torid / demolisher / cycle、Fulmin Prime / heavy gunner / semi-auto、Kuva Hind / standard multi target / semi-auto。未训练武器覆盖 Boar Prime、Ocucor、Kuva Nukor、Amprex、Tenet Arca Plasmor、Phantasma Prime、Strun Prime、Kuva Bramma、Xoris、Laetum、Lex Prime。

这些是固定本机负载的吞吐，不是服务器实际认可任务数。短行和桌面背景负载会引入波动，完整 4 行的 16 线程 PGO 样本为 0.501～0.596 s；不能把总量增幅当作每把武器的固定增幅。原始各轮时间、输入和 DLL SHA-256、逐行中位数在 [`experiments/hardware/results.json`](../experiments/hardware/results.json)。

对正式 WASM 的独立验证为 **41 行逐位一致、跳过 0 行、紫卡搜索一致**，涵盖 score 位模式、metric、完整 counts、gas/work；并非仅比较显示出来的分数。

另外执行了原生 quick-fleet 的完整流程，103 轮交互、20,880 场评分、work `9075847355` 及每轮 lead/score 报文摘要全部一致。这组单轮耗时为基线 3.793 s、PGO 3.502 s，用于回归，不作为紫卡普遍提速比例。

## PGO 的实现与复现

PGO 将真实运行的分支与调用频率反馈给 LLVM，帮助安排内联、分支和代码布局；它不更改战斗规则、随机序列、浮点精度或 gas 定义。此版本不使用 fast-math。机制参见 [Rust PGO 文档](https://doc.rust-lang.org/rustc/profile-guided-optimization.html)。

`tools/build-native-engine.ps1 -Pgo` 执行以下完整过程：

1. 在独立上游 worktree 应用已有补丁，以 ThinLTO 和插桩构建。
2. 使用本地榜单输入训练，不连接派发服务、不领取任务。
3. 用当前 Rust 工具链的 `llvm-profdata` 合并 profile，再优化构建。
4. 重新计算训练输入，并与插桩版本保留的完整答案对照；不一致即失败。
5. 在引擎构建清单记录训练输入 SHA-256、profile SHA-256、编译器版本、训练参数。

显式使用编码后的 Cargo 参数传递 profile 路径，支持 Windows 带空格目录。输出目录中的编译和验证材料不加入用户发行包。PGO 是可选构建参数，普通 CI 的构建成本不受此开关影响。

```powershell
rustup component add llvm-tools
./tools/build-native-engine.ps1 -Pgo -Strip -Cpu x86-64-v3 `
  -Source .wfsim-src -Commit 3747fab2571809ae1920bf1f20afaf87e1b6ee0b `
  -Engine 99dec9ad6d01 -Out dist/pgo
```

已有构建的独立对照：

```powershell
python tools/bench-board.py --library baseline=old.dll --library pgo=new.dll `
  --orders experiments/hardware/board-orders-holdout.json `
  --out comparison.json --lanes 8,16 --runs 128 --repeats 3
```

去掉 `--runs` 则使用上游行自己的完整 run 数。重新训练必须使用对应引擎的合法输入，并另留未训练样本检查泛化；profile 不应跨上游版本盲目复用。当前实际验证的平台为 Windows x64。

## CUDA 原型实际做了什么

`experiments/hardware/` 的 CUDA 原型通过 CUDA Driver API 和 NVRTC 在 RTX 5080 执行两个真实上游原语：

- SplitMix64 随机流及 53 位浮点转换：4 组 seed/offset，共 262,144 个数逐位一致，覆盖整数回绕。
- `rules::space::first_hit`：按原目标顺序寻找射线首次命中的圆形目标，返回目标索引、距离位模式和 gas。测试含退化方向、切线、边界附近、未命中、极大和极小方向。

CPU 参考 DLL 直接调用对应上游 Rust 函数。CUDA 保留 FP64，关闭 FMA 合并，按 libm 0.2.16 的原运算顺序实现 `hypot`，相应 MIT 许可证随源码保留。当前上游还给比较闭包单独计 gas，CUDA 也在相同条件下计量，不能只按循环次数估计。NVRTC 和 Driver API 参见 [NVIDIA 官方文档](https://docs.nvidia.com/cuda/nvrtc/index.html)。

下面是**合成射线输入、固定 361 个目标**的原语测量，7 轮中位数。CPU 多线程包含 Python 线程池分派与 FFI 调用，GPU 包含 Python 驱动调用；这不是单纯比较两种硬件的算术峰值。

| 每批射线 | CPU 单线程 | CPU 16 线程 | GPU 数据已驻留 | GPU 含射线上传和结果回读 |
|---|---:|---:|---:|---:|
| 32 | 0.0158 ms | 0.4211 ms | 0.1204 ms | 0.1220 ms |
| 256 | 0.1111 ms | 0.3280 ms | 0.1345 ms | 0.1402 ms |
| 4,096 | 1.8811 ms | 0.4498 ms | 0.1525 ms | 0.1800 ms |
| 65,536 | 26.1169 ms | 4.1551 ms | 0.8362 ms | 1.0823 ms |

固定目标坐标上传和一次性编译不在每批时间内；驻留列仍包括发射与同步，不能称为纯 kernel 时间。GPU 的大批优势已经测到，小批则明显不划算。现有生产模拟不会自然地在一个调用处聚齐 65,536 条独立射线，因此 **3.84× 原语加速不能外推成 3.84× 榜单加速**。

无需全局安装 CUDA SDK，原型依赖可局部安装：

```powershell
python -m pip install --no-deps --target .bench-hardware/cuda nvidia-cuda-nvrtc-cu12==12.9.86
./experiments/hardware/build-reference.ps1 -Source .wfsim-src -Out .bench-hardware/cpu-reference
python experiments/hardware/probe.py `
  --reference .bench-hardware/cpu-reference/cpu-reference.dll `
  --nvrtc-dir .bench-hardware/cuda/nvidia/cuda_nvrtc/bin `
  --out .bench-hardware/cuda-results.json
```

参考源码必须是报告对应的上游版本。当前探针使用 Windows NVIDIA 驱动，不包含 AMD 显卡路径；CUDA 上下文及显存分配在结束时释放。

## 没有推广的实验

**几何缓存。** `geometry-cache.patch` 缓存每线程最近一组不可变邻居关系和链式布局，按完整输入位模式索引、回放原 metered 函数的实测 gas。缓存有容量上限并随线程销毁。答案一致，但真实 25 目标榜单中整体慢约 1%，因此只留在实验目录，不进入原生构建补丁集。

**仅 ThinLTO。** 未训练武器组的 16 线程总时间为基线 3.120 s、ThinLTO 3.060 s、PGO + ThinLTO 2.589 s。主要收益来自 PGO。

**本机专用指令集。** `target-cpu=native` 的 PGO 构建在同一组只比 x86-64-v3 快约 1.6%，两组时间范围重叠。当前交付采用通用 AVX2 目标，专用版未替换它。

## 完整 GPU 后端的下一条验证边界

有效方向是把一批独立 run 的完整战斗状态驻留 GPU，由 CPU 准备输入、维护其他任务并按原索引顺序合并分片。频繁同步某一次伤害公式无法利用这里测到的大批优势。

必须先覆盖一个完整、明确的规则集合，并对全部 run 结果、随机流、计量和最终分数做参考对照；不支持的配置回到 CPU。按武器名称判断不足够，同一武器的 mod、目标、变形和异常状态配置可以触发不同机制。已有探针只解决其中的随机流、部分精确数学与几何，不覆盖状态机、持续伤害、换弹、变形、多目标结算和完整计量路径。

GPU 进入客户端前的验收指标是代表性完整榜单的 CPU-only 与 CPU+GPU 总 work/s，包含准备、传输、合并和任务长尾，并验证持续运行时显存和内存有界。原语测试通过不满足这条上线条件。

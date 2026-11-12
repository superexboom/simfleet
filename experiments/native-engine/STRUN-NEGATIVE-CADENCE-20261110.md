# Strun 系列负射速导致模拟不结束

> 2026-11-12 更新：上游已在 `e773e8be43d213bc06a513f177df8070034e99c3` 修复速度时钟，当前引擎 `5ddb771d2cb1` 包含修复。正式构建已移除下文的临时拒绝补丁，改用上游行为；捕获输入保留为原生/WASM 回归向量。下文是 11 月 10 日现场与临时补丁的历史记录。

核实时间：2026-11-10（北京时间）。这是运行前拒绝坏输入的临时保护，尚未发布到 Release；没有修改游戏射速规则，也没有给异常任务返回分数。

## 确认结果

用户副机的客户端是已发布的 `0.9.0-preview.20`，引擎为 `c43203504fc4`。两次采集为同一进程 PID 24668；10 个计算线程持续消耗 CPU。第二次采集的这些线程均在引擎 DLL 内执行。

- 原 DLL SHA-256：`5322667a3263cc62b74d613268a88d7e06759807a1b18ea46d7b849d600bbb17`
- 对应源码：`9c41dd3412cf4769007cb556041165ca794c96f7`
- 官方 WASM：`e383b2f5c62d`；数据：`13a8713ea68e`
- 用户采集包 SHA-256：`c7c87598d55c5f0ed4674a1a74ecb42ecf82bc215cfedf2b86a8fff445e1b3e0`

现场的 `request-002.json`（与 request-007 数值等价）已经独立复现，输入保存在 [negative-cadence-request.json](negative-cadence-request.json)。它实际是 MK1-Strun、cycle、9999 级卓越者重型机枪手；并非先前日志最后显示的 Strun Wraith。这不表示亡魂没有问题，而是本次拿到的确定复现输入属于 MK1。不能按武器名简单黑名单。

## 根因和证据

此配卡的射速桶：

| 来源 | 修正 |
| --- | ---: |
| 紫卡负射速 | −72.899998542% |
| Tainted Shell | −55% |
| Critical Deceleration | −19.98% |
| Magnetic Strafe | +40% |
| 合计 | −107.879998542% |

`engine/src/build/loadout/resolve.rs` 的 `fire_rate = base.base_fire_rate * (1 + fr + evo_fr_bonus)` 未限制这一输入。基础形态约为 −0.1641666363 发/秒，进化形态约为 −0.1181999781 发/秒。

`engine/src/model/weapon.rs` 的 `RefireRule::Rate::interval` 直接返回 `1 / rate`；`fight/after.rs` 将间隔加到模拟时钟。该输入可以不断把时钟倒退，结束条件始终无法正常到达。多个模拟分片同时进入这一情况，耗满所有计算线程。

复现调用：`/api/board/runs`，body 为 `{ "request": <附件请求>, "from": 0, "count": 1 }`。

- 已发布原生 DLL：第一局运行超过 12 秒仍未返回，独立重放进程被宿主超时终止。
- 官方原版 WASM：同一局耗尽 200 亿条 Wasmtime fuel，回溯位于模拟路径。该限制只在离线诊断器中设置，未改 WASM 字节。
- 仅移除 `tainted_shell` 的对照输入：原生与官方 WASM 均完成，完整 shard 相等。

结合负时间间隔的源码，确认这是上游模拟的终止性缺陷，不是我们原生优化独有的结果偏差。客户端原生线程没有硬超时隔离，会使异常局占住全部计算线程，放大为整机不再完成新任务。

2026-11-10 检查到的上游 HEAD `7f5012c128ad0d79d28a5f7e756b48eb47a77019`，上述相关文件相对该引擎源码没有修复。

## 临时保护

新增 `native-engine/patches/007-reject-nonpositive-board-cadence.patch`：

- `PreparedSimulation::new` 检查使用 Rate / NativeRate 的非正或非有限射速，递归检查循环形态、切换形态与其他参与者。
- `/api/board/order` 提前执行准备检查。这样现有客户端可以走 `ok:false` 的“跳过＋报告拒绝”路径，不会等到全部线程被坏局占满。
- 直接调用 prepared / board-runs 也会拒绝。
- 不进行射速 clamp、不替换配卡、不制造分数，不改变有效模拟的算术或 gas 计量。
- 此策略可能保守地拒绝可由动态增益恢复的非正初始射速任务；它是防卡止损，不是上游游戏机制的最终修复。
- 无法中断已经进入旧 DLL 的计算；需退出旧实例后更换 DLL 并重新启动。

构建脚本在使用 upstream-prepared 时要求该保护补丁成功应用，避免上游漂移后静默丢失保护。

## 必要验证

只增加一个真实现场回归用例 `board::tests::negative_board_cadence_is_refused_before_claiming_lanes`，检查准备拒绝、任务入口拒绝和 opaque ABI 不产出分数，已通过。

正常对照输入 1,000 局（4 线程）在旧 DLL 与保护版上得到完全相同的 score、work、counts；保护版的单局 shard 与官方 WASM 完全相同。没有跑全量测试，也没有运行 GPU 构建。

本次临时 DLL 为 Windows x86-64-v3 / AVX2、非 PGO 构建，保留当前 ABI/文件名以供副机手动试用；正式发布应按原有 PGO 流程重新构建，并使用新的原生修订号。未替换本机部署文件或修改远端 Release。

保护版 DLL SHA-256：`baaa3b7ae1b9dc37c72e8ec11d6b105b268f896f9e24c41a64147f97c8ec1c32`。

## 上游需要决定的修复

正式修复应在共享模拟入口约束非法速率/非单调时间，并依据机制证据确定射速下限或无法开火的行为，同时覆盖基础/进化形态。不能由客户端自行选择一个最小射速后提交与官方不同的分数。

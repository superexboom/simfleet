# 2026-10-31 原生引擎 Action 修复

线上引擎 `ea828b371375`，WASM `d00fbb5dd94b`，数据 `40fe1aa06a49`；未修改上游提交 `e5791136347341e7435d8a5623aaa836dd87350f` 的引擎号与线上一致。

## 失败原因

Action `41180872549` 的 Windows 任务在原生构建准备阶段失败，错误为 `PGO requires the opaque board ABI patch`。上游 `webapi/src/lib.rs` 在原有模块列表插入 `scenario_catalog` 和 `static_stats`，本地 `001-prepared-board.patch` 依赖邻近多行上下文，导致 `git apply` 失败。旧脚本把这个补丁当可选项跳过，但 Windows PGO 训练必须使用其 prepared-board 接口，随后主动终止。因此反复重跑旧代码无效。

Linux/macOS 当次已完成并通过官方 WASM 核对；因为不要求 PGO，发布了缺少 prepared-board 快速接口的 sfe2 库。已发布文件保持不变，本轮先补齐缺失的 Windows PGO v3 文件，避免覆盖用户已验证的引擎资产。

## 改动

- 模块注册补丁只依赖相邻的 `rivens` / `shapley` 声明，不再绑定 `simulate` 周围会增补的模块；prepared-board 的计算实现不变。
- `001-prepared-board.patch` 对所有经过补丁优化的原生构建均为必需项。应用失败立即终止并点名补丁，避免其他平台悄悄失去批次加速；`-NoPatch` 仍可显式构建上游参考。
- 内存生命周期补丁继续强制要求。`005-deferred-lead-reports.patch` 仍为可选，其不适用并非此次失败原因。

## 核对

旧补丁在当前源上实际返回 patch does not apply。更新后仅核对当前 `ea828b371375` 和先前 `14c617506965` 两份干净源码，两者均可应用。后续构建只运行与产物有关的 PGO 一致性和 WASM 核对，不追加客户端全量测试。

上游另有未量化状态权重、默认精度统一、独立复核与数据库查询维护等变动，均随精确提交构建。CPU 与 GPU 引擎号必须同步；旧 GPU 包不能改名冒充新引擎，新的 NVIDIA 构建范围仍为 sm120（RTX 50 系列）。

## 客户端协议边界

上游新增完整计算报告任务（`calculation-reports-v1`），必须主动声明能力才会派发；现有 simfleet 不声明该能力，继续处理已有榜单和紫卡任务，不能只添加能力标记就冒充支持。协议版本仍为 8。本轮无需更改客户端协议或重新发布客户端。

上游的派单 SQL 还改为先按 question 定位答案，再过滤 engine，避免新引擎发布后统计尚未更新时扫描整日答案。该改动属于服务端；此次 Action 失败发生在本地补丁应用阶段，与它无关。

## 完成结果

修复后的 Action `41205099682` 已成功，Windows PGO v3 与同提交官方构建方式产生的 WASM 在 8 行上分数、work、gas 逐位一致，紫卡 canary 一致。已发布缺失的 Windows DLL、SHA-256 与构建清单；下载附件的哈希与 GitHub digest 和清单全部一致，原有 Linux/macOS 九个附件的 digest 未改变。

新 sm120/sfg3 GPU 已在 RTX 5080 核对 128 分片、两条短评分和四条完整 1000 次榜单评分，全部与匹配 CPU 引擎一致。构建进程组峰值约 4.4 GiB；四行 worker 核对的私有提交约 3.59 GiB，不代表所有场景的内存或吞吐保证。

新 DLL、GPU 六文件套件及已核对官方 WASM/数据包已放入部署目录。客户端、配置、引擎选择状态和学习文件的哈希不变；任务开始时没有运行实例，本次保持停止，下一次启动由客户端执行常规 canary 并启用当前引擎。

[原生引擎](https://github.com/superexboom/simfleet/releases/tag/engine-ea828b371375) · [sm120 GPU 套件](https://github.com/superexboom/simfleet/releases/tag/gpu-ea828b371375-sfg3) · [修复后的 Action](https://github.com/superexboom/simfleet/actions/runs/41205099682)。详细哈希与核对范围见 `experiments/gpu-board/upstream-ea828b371375-validation.json`。

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

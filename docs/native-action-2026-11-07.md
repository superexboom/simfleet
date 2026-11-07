# 2026-11-07 CPU 引擎构建跟进

线上引擎 `d8787cb351c5`，官方 WASM `fb2253634d34`、数据 `bb069d7aba3c`；干净上游提交 `1761c310530fd4f11ef44b08398e83ed6be689cc` 计算出的引擎号与线上一致。本轮仅构建 CPU 原生引擎，不构建 GPU。

## 原因

11 月 5 日 Action `42067202509` 和 11 月 7 日 `42325775931` 的原生编译均出现 E0425：`engine/src/data/weapons/panel.rs` 找不到 `raw`。后一个 run 总状态为 cancelled，但 Windows/Linux 已先因同一错误失败；总状态不能代替具体 job 日志。

本地 `003-static-panel-metadata.patch` 原先从 `base_panel_assembled` 的 `raw` 借用静态目录字符串和 MOD 池，防止每次面板构造都调用 `Box::leak`。上游把面板主体移动到递归使用的 `panel_for_spec`，原补丁文本依旧可以应用，但借用变量不在新作用域。因此只检查 `git apply --check` 无法捕获这类兼容错误，反复重跑也无法修复。

## 修复与范围

在实际构造 `WeaponBase` 前，通过 `s.id` 重新取得静态目录元数据，继续使用目录字符串引用和每种武器只创建一次的 MOD 池切片。普通面板、组合武器和递归的攻击 profile 都走同一条路径；不修改战斗、随机种子或 gas 算法。

新增一个原生测试，检查普通武器、默认组合武器和一个实际包含攻击 profile 的面板：元数据内容与目录相同，两次构造及嵌套面板复用同一内存。仅运行该相关用例；正式产物继续执行既有 PGO 与 WASM 一致性核对，不追加客户端全量测试。

上游同时更改了紫卡优化器的计划和候选身份（加入固定槽位、组合部件）。旧的可选 `004/005/006` 补丁已不能套用，构建明确跳过，使用上游路径；不强行恢复旧身份规则。本轮保证 prepared-board、单线程优化器入口和内存生命周期补丁继续生效。缓存未启用可能影响紫卡吞吐，未进行跨版本性能基准。

本轮没有客户端版本变化；已停止的部署实例保持停止，正式 CPU DLL 在验证后单独放入其 engines 目录。旧 GPU 包保持原引擎标识，不能用于新引擎。

## 执行器退役

修复编译问题后，Action `42336910273` 的 macOS job 在 1 秒内报 `The configuration macos-14-arm64-us-default is not supported`。GitHub 官方 runner-images 清单确认 macOS 14 于 2026-11-02 退役；改用仍受支持且为 arm64 的 `macos-15`。原生构建、客户端发布和普通 CI 的三个引用一并更新，避免下次客户端发布再次遇到相同基础设施故障。

Windows/Linux 已启动的构建继续完成，不取消重算；后续 run 根据缺失附件只补建未发布平台。执行器来源：https://github.com/actions/runner-images/blob/main/README.md 。

## 发布阶段恢复

首轮 Windows PGO 与 Linux baseline/v3 已通过构建和 WASM 核对，但创建 Release 时工作流令牌返回 HTTP 403 `Resource not accessible by integration`。构建过程中主分支加入了执行器迁移，发布目标仍是旧提交；这一情况与 GitHub 对含工作流差异的旧提交创建 release 的令牌限制相符。未修改仓库令牌权限或配置额外凭证。

下载首轮已校验产物，用当前仓库登录直接发布同一文件，核对 GitHub digest、`.sha256` 和构建清单后，再启动只缺 macOS 的 Action `42338859448`。最初已排队的 `42337130346` 因尚未看到这些附件会重复编译，因此在执行前取消。首轮成功的平台没有重算。参考：https://github.com/cli/cli/issues/9514 。

北京时间 12:32:44，Windows PGO v3 DLL 及官方 WASM/数据包已放入部署目录，配置、客户端和 GPU 文件未改动，程序保持停止。下次启动执行客户端原有 canary 并选择当前 CPU 引擎。

## 最终结果

Windows PGO v3、Linux baseline/v3、macOS Apple silicon 四份二进制及对应的 SHA-256/构建清单已全部发布。各构建均通过 8 条榜单行的分数、work、gas 与紫卡 canary 对照；下载后的 12 个附件与 GitHub digest、二进制清单一致。macOS 补建 Action `42338859448` 整体 success，自动追加附件也成功。

本机验证仅增加面板内存复用这一条原生用例，未重复客户端测试或跑长期压力测试。三平台编译排除了原先的变量作用域错误，普通/组合/嵌套面板测试确认同一目录项继续复用静态元数据。未构建、替换或发布 GPU。本机仍为停止状态，首次运行加载时的客户端 canary 尚由用户下次启动执行。

[引擎 Release](https://github.com/superexboom/simfleet/releases/tag/engine-d8787cb351c5) · [成功的补建 Action](https://github.com/superexboom/simfleet/actions/runs/42338859448)。详细哈希在 `experiments/native-engine/upstream-d8787cb351c5-validation.json`。

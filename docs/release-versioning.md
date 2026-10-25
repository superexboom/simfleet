# 客户端、CPU 引擎和 GPU 引擎的版本

三个版本号各自描述一层兼容性。GPU 是客户端的可选计算后端，不另做一个内容几乎相同的“大 GPU 客户端”。同一个客户端在没有显卡、没有匹配 GPU 包或者 GPU 失败时继续使用 CPU。

| 层 | Release / 文件示例 | 版本改变的条件 |
|---|---|---|
| 客户端 | `v0.9.0-preview.16`；`simfleet-0.9.0-preview.16-windows-x64.zip` | 界面、调度、协议、回退和资源管理改变 |
| CPU 引擎 | `engine-3194962a0af7`；`3194962a0af7-sfe2-v3.dll` | 上游引擎哈希；本地包装和构建修订；CPU 指令级别 |
| GPU 引擎 | `gpu-3194962a0af7-sfg3`；`simfleet-gpu-3194962a0af7-sfg3-sm120-windows-x64.zip` | 上游引擎哈希；GPU 包格式/实现修订；GPU 架构 |

上游引擎标识不能拿客户端版本号代替。`sfe2` 将 Windows 默认构建切为 PGO＋ThinLTO v3，并保留新文件名避免覆盖已发布的 sfe1；Linux 仍构建 baseline/v3，macOS 构建 Apple silicon。`sfg3` 是 GPU 包的协议/实现修订，使用 1 KiB 私有页和 16 位页索引，需要 preview.14 或更新客户端。`v3` 指 x86-64-v3 主机指令集，`sm75` / `sm89` / `sm120` 指 NVIDIA GPU 架构，两者不能混称。GPU 包内的 `wire.dll` 也需要 x86-64-v3。

普通用户仍只下载自己平台的客户端。Windows 有兼容 NVIDIA 显卡、愿意试用 GPU 的用户再下载单独 GPU 包，直接解压到 `simfleet.exe` 所在目录。在设置菜单中打开 GPU 开关；配置会即时保存。目前只读本地 GPU 包，不会自动下载实验性 GPU 二进制。分发按引擎、操作系统、主机指令集和 GPU 计算能力选择：GTX 16 / RTX 20 对应 sm75，RTX 40 对应 sm89，RTX 50 对应 sm120；不按单张显卡型号制作套件。

当前更新默认只构建和发布 **sm120（RTX 50 系列）**。sm75、sm89 的历史包保留，但不随新引擎自动重编；其历史包只能用于原来匹配的 engine ID。只有另行要求相应架构时才新增构建，不将旧包改名为新引擎。

GPU 包有六个文件：`engine.cubin`、`bridge.wasm`、`wire.dll`、`engine.gpu.json`、`LICENSE` 和 `NOTICE.txt`，都位于 `engines/gpu/<engine>-sfg3-sm120/`。后两份仅保留必要许可。技术报告、基准输入、验证结果、编译器、CUDA SDK 都不进入发行包。客户端仍只有可执行文件、README 和三份许可证/署名文件；CPU DLL 保持独立引擎 Release。

加载器先核对 schema、GPU 修订、引擎标识、主机类型和三个二进制 SHA-256，再检查 GPU 架构。任一不匹配就回退 CPU。包内哈希用于完整性与版本绑定，不构成发布者的数字签名，仍应从项目的正式发行渠道获取模块。

预览阶段使用 `v0.9.0-preview.N`，在 GitHub 标记 Pre-release 且不设置 Latest；验证稳定后发布 `v0.9.0`。单独重编译 GPU 包不要求升级客户端，但影响二进制协议、正确性或选择策略的变化应提升 `sfg` 修订并更新客户端接受的修订。稳定客户端、预览客户端以及 GPU/CPU 引擎分别发布，避免把引擎包误当客户端更新。

打包命令：

```powershell
./tools/package.ps1
./tools/package-gpu.ps1 -Source output/gpu-board-3194962a0af7-sm120
```

正式发布前需要提交完整对应源码，并上传已核对哈希的可选 GPU 包。客户端发布检查只运行与变更直接相关的用例；仅更新引擎时，不重复客户端全量测试。原生引擎执行必要的 WASM 结果一致性校验，GPU 执行对应设备的分片和完整任务核对。sm75、sm89 交叉编译包标记 `device_validated: false`，需要对应架构实机验证；sm120 的验证结果不能代替其他架构验收。增加同一 GPU ABI 下的架构包不要求更新客户端版本。三个发布层都保留完整对应源码和校验信息，客户端压缩包不加研究材料。

# NVIDIA 测试包安装

客户端：`v0.9.0-preview.7`。CPU 原生引擎：`engine-4296a0964da6`，Windows 为 `4296a0964da6-sfe2-v3.dll`，使用 PGO＋ThinLTO。GPU 引擎：`gpu-4296a0964da6-sfg1`。三个 Release 分开下载，不把引擎二进制塞进客户端压缩包。

1. 解压 Windows 客户端包到副机独立目录。不要直接复制主机的 `simfleet.toml`、计算身份或任务日志；第一次启动按正常流程配置副机身份、账号和核心数。
2. GTX 1660 Ti 使用 `simfleet-gpu-4296a0964da6-sfg1-sm75-windows-x64.zip`。RTX 5080 使用 `sm120` 包。把选中的 GPU zip 解压到 `simfleet.exe` 同目录，得到 `engines/gpu/4296a0964da6-sfg1-sm75/` 等目录。包内只有三个二进制、清单、LICENSE 和 NOTICE。
3. 使用可用的 NVIDIA 显卡驱动；无需另装 CUDA Toolkit、Python 或编译器。包使用 CUDA 12.9 工具链生成，需要支持该 cubin 的驱动；太旧的驱动可能加载失败。
4. 启动客户端，等同版 PGO 原生引擎自动下载并通过参考校验。在 `S → NVIDIA GPU 预览` 开启。默认 `S → CPU+GPU上限 MiB` 为 8192；这是系统提交内存，不含显存。客户端还会根据显卡可用显存限制批量。
5. 看“GPU状态”“程序GPU”和“贡献 → 其中GPU”。`探测计算` 只学习适用性，完成正常任务后才增加 GPU 贡献。等待、暂缓或没有同版 GPU 包时由 CPU 继续计算。

sm120 在 RTX 5080 上做实机参考验证。sm75 是交叉编译测试包，清单明确记录 `device_validated: false`；不能声称已在 1660 Ti 测过或保证有吞吐收益。每个新场景仍做原始 WASM 分片校验，完整任务仍做 canary；不一致会停用 GPU，临时资源故障会回 CPU 并恢复。

GPU 架构和上游引擎都必须匹配。程序会自动从已安装的包中选择本机架构，但目前不自动下载 GPU 包；上游引擎变化后需要对应版本套件。不要改目录名、清单架构或引擎号尝试让旧包冒充新包。

副机出现问题时，保留目录中的 `dispatch.log`、`gpu-diagnostics/last-failure.json` 和 `gpu-diagnostics/previous-failure.json` 即可追查。诊断不包含登录凭证；不要公开 `simfleet.toml` 或任务身份文件。关闭 GPU 开关可继续原生 CPU 计算，保留套件目录不会强制启用。

贡献统计的口径：总“速度”按本机 CPU＋GPU 最近十分钟已完成工作量计算；“其中GPU”是同一窗口里的 GPU 部分，“平均”包含本次运行的启动和等待时间。账号榜单旁的速度是最近约半小时入账增量，可能包含多台设备并滞后于本机完成时间。

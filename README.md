# simfleet

**WFSim 的志愿计算客户端**：把 [wfsim.app](https://wfsim.app) 网页里「帮 WFSim 计算」的那部分单独拿出来，做成一个命令行小程序。开着它，这台电脑就会按你给的预算，替 WFSim 计算排行榜分数和紫卡增益——跟开着网页一样，只是更快、更省、能常驻。

> 非官方项目，**基于 [WFSim](https://github.com/magenie33/wfsim)**（作者 magenie33，AGPL-3.0-or-later），与 WFSim 官方、Digital Extremes 均无关联。按 WFSim 的 [商标政策](https://github.com/magenie33/wfsim/blob/main/TRADEMARK.md)，衍生项目不能以 “WFSim” 命名，所以叫 simfleet。署名与许可详见 [NOTICE.md](NOTICE.md)。

```
14:18:37 [开始] 紫卡增益  dual_toxocyst · standard_single_target [SD00N9]
14:22:11 [完成] 紫卡增益  dual_toxocyst · standard_single_target [SD00N9]  最佳 96.71 kpm · 试了 6,038 个构筑 · 345.53 分 · 214s
14:22:12 [开始] 榜单  exergis · standard_single_target · base  1000 次模拟
14:22:14 [完成] 榜单  exergis · standard_single_target · base  = 94.2308 kpm · 8.64 分 · 2.2s
───────────────────────────────────────────────────────────────────────────────
 ● 计算中 · 8/16 核(使用中) · 本次 65 任务 737.7 分(已确认 27/待确认 13) · 今日 170 分 · 本机累计 170 分(未登录) · 全站在线 51 · native a4cf7731
 榜单 burston_prime · heavy_gunner · cycle  [############--------]  61% (611/1000)   [P]暂停/继续 [+/-]空闲核心 [Q]退出
```

（测试运行时的真实输出。）上面滚动的是任务日志，最下面三行常驻：分隔线、状态与贡献总览、当前任务进度和按键。登录后「本机累计」换成账号名、账号累计点数和贡献者段位。

## 上手

**下载**：在 [Releases](https://github.com/superexboom/simfleet/releases) 下载 `simfleet-<版本>-windows-x64.zip` 并解压。只需要 Windows 10/11 x64，不需要装 Rust 或任何运行库。

1. 把 `simfleet.exe` 放进一个文件夹（这个文件夹就是安装目录，配置和缓存都在里面；删掉文件夹就是卸载）。
2. 双击运行。第一次会问三件事：是否同意计算、离开电脑时用几个核心、要不要登录。
3. 之后每次双击就开始算。按 **P** 暂停/继续，**+ / -** 调整空闲核心数，**Q**（或 Ctrl+C、直接关窗口）退出——退出前会把手头的任务交还服务器，不会让任务白白挂 30 分钟。

不登录也能算：贡献先记在本机的计算者 id 上，**以后第一次登录时，之前攒的点数会一起转到账号名下**。

```
simfleet                 开始计算
simfleet --cores 8       本次空闲时用 8 个核心
simfleet setup           重新设置预算
simfleet login           邮箱 + 密码登录
simfleet login --cookie <值>   用 Google/Discord/GitHub 登录的账号（见下）
simfleet logout          退出登录
simfleet status          查看本机和账号的贡献
simfleet bench           本机测速（不领任务）
```

**第三方登录的账号**（Google / Discord / GitHub）没有密码可输：在浏览器登录 wfsim.app → F12 开发者工具 → Application/存储 → Cookies → `https://wfsim.app` → 复制 `wfsim_session` 的值 → `simfleet login --cookie 那个值`。

## 预算与占用

| 设置（`simfleet.toml`） | 默认 | 含义 |
| --- | --- | --- |
| `idle_cores` | 核心数 × 30%（向上取整） | 离开电脑时用几个核心 |
| `active_cores` | 1 | 你在用电脑时用几个核心；`0` = 暂停 |
| `idle_after_secs` | 60 | 键盘鼠标多久没动算「离开」 |
| `pause_on_battery` | true | 用电池时暂停 |
| `priority` | `idle` | 进程优先级：`idle` 只吃没人要的 CPU，`below_normal`，`normal` |
| `native_engine` | true | 有匹配的原生引擎时，榜单任务用原生（见下） |
| `slots` | 0（自动） | 同时开几路（几个计算者 id），见下文「调度」；填数字就固定开几路 |

跟网页一样的规矩：没有你的同意不算；用电时算多少由你定；每份结果都要另一个**不同主人、不同网络**的电脑算出一模一样的数才算数，服务器还会抽查 5%。

## 调度：把预算里的核心用满

一个任务很难独自吃满一台机器。紫卡增益的快速下降法是「领头一步（单核）→ 本轮所有构筑分片打分（多核）→ 领头一步……」的链条，领头那几步只有一个核心在算；榜单任务能把 1000 次模拟摊到所有核心上，但结尾总有拖尾，任务之间还隔着一次网络往返。实测 16 核只跑一个任务时，**核心利用率只有 55%**。

simfleet 用的是分布式/批处理调度里的成熟做法：

- **多任务并行**：服务器允许一个计算者 id 同时持有「正在算的 1 个 + 提前领的 1 个」（`TASKS_AHEAD`），simfleet 两个一起算，而不是算完一个再算下一个。
- **多路（slots）**：一个浏览器就是一个 id，玩家本来就会多开浏览器来提高并发；simfleet 同样可以开几路，每路一个自己的 id，登录后都绑到你的账号（设备列表里显示为 `simfleet · 主机名 #2` 等）。服务器不让同一主人或同一网络的两个 id 互相确认，所以多开只增加算力，不增加「见证人」，不影响结果的可信度。
- **一个共享的核心池，先来的任务优先**（类似 Slurm 的 backfill / 工作窃取）：每个空闲核心去拿最早那个任务的下一块活，它用不上的核心自动补给后面的任务，核心永远不等任务。
- **紫卡增益的领头步骤固定在它自己的核心上**：搜索的状态存在那个 wasm 实例里，所以那个核心不接别的任务的 wasm 活（原生引擎的榜单块不碰 wasm，可以接）。
- **网络从不阻塞计算**：领任务、续租、交结果都在后台线程；「不带 ahead 的领任务」会让服务器收回这个 id 手上的任务，所以只有在这个 id 手上和路上都没有任务时才发。
- **开几路是测出来的，不是猜的**（反馈控制）：每 30 秒看一次，利用率低于 85%、每一路都有活、服务器也还有活，就加开一路（最多核心数的 1/4）；核心已经满了、又有任务被饿着，就让最新的一路做完手头的活后不再接新活。

同一台机器（16 线程）、同样 16 核预算、各跑 10 分钟、都是紫卡增益调查任务：

| | 旧：一次一个任务 | 新：自适应多路并行（自动开到 2 路 = 4 个任务） |
| --- | --- | --- |
| 核心利用率 | 55% | **96%** |
| 完成的紫卡增益 | 7 个 | **12 个** |
| 提交的点数 | 约 1,133 | **约 1,951（+72%）** |

代价是单个任务变慢（大家分核心：单个紫卡增益从 44–163 秒变成 69–270 秒），换来总吞吐提高约七成。所有算法步骤与网页完全相同：新调度下的榜单和紫卡增益结果都和网页算法逐位一致（同样的构筑、分数、work、对战数）。

想固定路数（比如只想一路、保持单任务速度），在 `simfleet.toml` 里写 `slots = 1`。

## Go 还是 Rust：实测

关键事实：网站对客户端结果做**逐位比对**，所以客户端必须跑和浏览器完全相同的引擎；而 WFSim 发布非常频繁，引擎号（源码哈希）一变，旧引擎就领不到任务。所以最省心的做法是**直接下载网站正在发布的那个 wasm 引擎来跑**——客户端本体不用跟着更新。宿主语言本身几乎不干活，速度取决于 wasm 运行时的编译器。

同一条真实榜单（Braton Prime · demolisher · cycle · 1000 次模拟，单核）：

| 运行方式 | 用时 | 相对浏览器 | 结果 |
| --- | --- | --- | --- |
| 浏览器 V8（Node 24 里跑同一个 wasm） | 4.36 s | 1.00× | 与榜单一致 |
| **Go** + wazero 1.9（纯 Go wasm 运行时） | 8.95 s | 0.49× | 一致 |
| **Rust** + wasmtime 49（Cranelift） | 6.90 s | 0.63× | 一致 |
| **Rust** 原生编译 WFSim 引擎 | 3.83 s | 1.14× | 一致 |

多目标榜单（standard_multi_target）：V8 13.7 s，wasmtime 18.0 s，原生 11.5 s，三者都与榜单逐位一致。

结论是 **Rust**：
- wasm 路线上 wasmtime 比 wazero 快约 30%，且对 `externref` 支持完整（Go 的 wazero 目前连导出函数都要绕坑）；
- 更重要的是 WFSim 引擎本身就是 Rust，只有 Rust 能把它**原生编译**进来——比 wasm 快 2 倍、比浏览器还快；Go 只能通过 cgo 绕一圈；
- 本体 9.7 MB，单文件、无运行时依赖。

simfleet 因此有两个引擎：
- **wasm（默认，永远可用）**：运行网站当前发布的 `/pkg/wfsim_wasm_bg.<摘要>.wasm`，按文件名里的 sha256 校验，编译一次后缓存；网站发新版时自动下载新引擎，客户端不用更新。
- **原生（自动，更快）**：`engines/<引擎号>.dll`，由本仓库的 Action 从 GitHub 上与网站引擎号完全相同的那次 WFSim 提交编译并发布，simfleet 自动下载（见下文「原生引擎」）。每次启动都会在一条金丝雀榜单上和 wasm **逐步逐位比对**，不一致就自动停用。只用于榜单任务；紫卡增益的搜索在原生下批宽不同，所以始终走 wasm，保证和浏览器志愿者算出一样的构筑。

本机（16 线程）实测，同一条榜单：

| | 1 核 | 16 核 |
| --- | --- | --- |
| wasm | 6.9 s（145 次/秒） | 0.95 s（1,057 次/秒） |
| 原生 | 3.8 s（261 次/秒） | 0.44 s（2,284 次/秒） |

紫卡增益（dual_toxocyst 一个真实调查任务）：网页算法在 V8 单线程 472 s；simfleet 16 核 117 s；**最终构筑、分数、work、对战数与网页算法完全相同**。

内存：原生引擎跑榜单几乎不占内存；wasm 每个核约 50 MB（和浏览器一个 worker 一样），空闲一分钟后自动释放。

## 原生引擎（自动）

不用自己编译。本仓库的 [native-engine](.github/workflows/native-engine.yml) Action 每 30 分钟计算一次 WFSim `main` 分支的引擎号（wfsim.app 对 GitHub 的服务器会弹出人机验证，所以 Action 完全不访问网站；网站发布的引擎号就是它所用源码的哈希）。遇到还没发布过的引擎号，它会在那次提交上，用 WFSim 自己锁定的 Rust 和 wasm-bindgen 版本：

1. 照 WFSim 的做法编译出 wasm，作为比对的参照；
2. **把 Digital Extremes 的卡牌文本清空**（`tools/strip_card_text.py`）后编译原生引擎。WFSim 对这部分文本没有授权（见其 [LICENSE-DATA.md](https://github.com/magenie33/wfsim/blob/main/LICENSE-DATA.md)），所以发布的版本里不含它；引擎只拿这些文本做显示，不影响任何分数；
3. 在 WFSim 仓库里提交的榜单快照上取 32 条真实榜单（4 把武器 × 4 把尺子 × 2 种模式），和参照 wasm **逐位比对**（`simfleet check-native`），全部一致才发布；
4. 发布为 Release `engine-<引擎号>`，附 sha256，说明里写明对应的 WFSim 提交、simfleet 提交和构建记录。

simfleet 只用网站**实际发布**的引擎号：启动时在 `engines\` 里找对应的 dll，没有就去 Release 下载，校验 sha256，再在金丝雀榜单上和**网站自己的 wasm** 比对一次，一致才用；运行中用 wasm 时每 30 分钟再看一次有没有发布出来。不想用原生引擎，把 `simfleet.toml` 里的 `native_engine` 设为 `false`；只想用自己编译的，把 `native_engine_url` 设为 `""`。

自己编译（要 git、Rust，`-Strip` 还要 Python）：

```
powershell -ExecutionPolicy Bypass -File toolsuild-native-engine.ps1 [-Strip]
simfleet check-native          # 和网站的 wasm 比对 32 条网站上的榜单
```

网站引擎更新后，旧的 dll 自动不再使用，回到 wasm，直到新的原生引擎发布出来。

## 怎么做到「和浏览器一模一样」

协议逐条照搬网页的 `69-board-work.js` / `08-checkpoint-api.js`（WFSim 源码）：

- `/api/board/work` 领任务（带同意书版本与时间、协议号 6、可用核心数），80% 进度时提前领下一个（`ahead`），中断就 `/api/board/release` 交还；
- 榜单：`/api/board/order` 取对战 → 各核并行跑 `/api/board/runs`（每局一个 shard）→ **严格按局序** `/api/board/fold` → `/api/board/score` → `/api/board/verify`；
- 紫卡增益：`quick_fleet` 下降，lead 永远在 0 号核的同一个实例上，评分切片每核两片，2 分钟续租一次，结果按 `boardPayloadFromResult` 组装后交 `/api/appraise/<code>/result`；
- 所有在引擎之间传递的 JSON 都按 `JSON.parse`/`JSON.stringify` 的语义处理（数字转 f64、`-0` 写成 `0`、整数键排前），保证喂给引擎的值与浏览器逐位相同。

## 构建

```
cargo build --release          # target\release\simfleet.exe
cargo test --release
powershell -File tools\package.ps1   # dist\simfleet-<版本>-windows-x64.zip
```

发布：推送 `v*` 标签后，GitHub Actions（`.github/workflows/release.yml`）会在 Windows 上从源码编译、打包，并把 zip 附到对应的 Release。

## 署名与许可

- **simfleet**：Copyright (C) 2026 superexboom，以 [AGPL-3.0-or-later](LICENSE) 开源。
- **WFSim**：simfleet 基于 [WFSim](https://github.com/magenie33/wfsim)（magenie33 与贡献者，AGPL-3.0-or-later）。任务协议和客户端逻辑移植自 WFSim 网页客户端的 `69-board-work.js`、`08-checkpoint-api.js`、`68-board-submit.js`，这部分版权归 WFSim 作者与贡献者。
- **引擎**：simfleet 本身不含 WFSim 引擎，运行时从 wfsim.app 下载官方发布的 wasm；原生引擎由本仓库的 Action 从 WFSim 源码编译（卡牌文本已清空），发布说明里写明对应源码。引擎及其数据按 WFSim 的许可（代码 AGPL；数据与引擎算出的分数等见其 [LICENSE-DATA.md](https://github.com/magenie33/wfsim/blob/main/LICENSE-DATA.md)，分数为 CC BY-NC-SA 4.0，需注明 WFSim）。
- **游戏数据**：由 WFSim 整理自 [WARFRAME Wiki](https://wiki.warframe.com/)（CC BY-NC-SA 3.0）。*Warframe* 是 Digital Extremes Ltd. 的商标。
- **第三方库**：wasmtime、ureq、rustls、serde、crossterm 等，许可证全文见 [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md)。

详见 [NOTICE.md](NOTICE.md)。

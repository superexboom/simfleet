# Notice / 署名

**simfleet** — a volunteer compute client for WFSim.
Copyright (C) 2026 superexboom.

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version. It is distributed WITHOUT ANY WARRANTY; see [LICENSE](LICENSE).
Source code: <https://github.com/superexboom/simfleet>

## Based on WFSim

simfleet is **based on [WFSim](https://github.com/magenie33/wfsim)** (wfsim.app)
by magenie33 and its contributors, licensed AGPL-3.0-or-later. It is an
unofficial, independent project: not made, endorsed or maintained by WFSim, and
it does not use the WFSim name or logo as its own (see WFSim's
[TRADEMARK.md](https://github.com/magenie33/wfsim/blob/main/TRADEMARK.md)).

What simfleet takes from WFSim:

- **Ported code.** The compute-order protocol and the client's side of it are
  ported from WFSim's web client — `web/src/static/app/69-board-work.js`
  (leasing, `measureRow`, asking ahead, releasing), `08-checkpoint-api.js`
  (`quickFleet`) and `68-board-submit.js` (`boardPayloadFromResult`) — and its
  `wasm-bindgen` host follows WFSim's `wasm/` bindings. These parts remain
  Copyright (C) magenie33 and WFSim contributors, under AGPL-3.0-or-later.
- **The engine, at run time.** simfleet does not contain the WFSim engine. It
  downloads the engine wfsim.app serves (`/pkg/wfsim_wasm_bg.<digest>.wasm`),
  checks it against the digest in its name, and runs it. That module is WFSim's
  work, under WFSim's licences (code AGPL-3.0-or-later; data and engine outputs
  as set out in WFSim's
  [LICENSE-DATA.md](https://github.com/magenie33/wfsim/blob/main/LICENSE-DATA.md)).
- **The native engine.** `native-engine/` is a thin wrapper that links
  WFSim's `wfsim-webapi` crate. The `native-engine` workflow builds it from the
  WFSim commit that carries the served engine and publishes it as the release
  `engine-<id>`, whose notes name that commit (the corresponding source). Before
  the build, **Digital Extremes' card text is blanked** from WFSim's `data/`
  (`tools/strip_card_text.py`): WFSim makes no grant for that text
  (LICENSE-DATA.md §4), so no published build carries it. The patches in
  `native-engine/patches/` are then applied to WFSim's `webapi/` (they change
  how fast a board row's runs are set up, never what they compute) and named
  in the release notes; the corresponding source is that commit, the card-text
  script and those patches. The engine only displays card text; every
  published build matched the wasm built from the
  same commit bit for bit on real board rows before release, and simfleet
  checks it against wfsim.app's own wasm before every use.

Scores and other figures simfleet computes are WFSim engine outputs; WFSim
licenses those CC BY-NC-SA 4.0 with attribution to WFSim.

## Game data and trademarks

Game data used by the WFSim engine is sourced by WFSim from the
[WARFRAME Wiki](https://wiki.warframe.com/) (CC BY-NC-SA 3.0).
*Warframe* is a trademark of Digital Extremes Ltd. simfleet is a fan project
and is not affiliated with or endorsed by Digital Extremes.

## Third-party crates

simfleet.exe is built with open-source Rust crates (wasmtime, ureq, rustls,
serde, crossterm and others) under Apache-2.0, MIT, ISC, BSD-3-Clause, Zlib and
similar licences. Their licence texts are in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md), regenerated with
`cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md`.

---

**中文摘要**：simfleet 由 superexboom 编写，以 AGPL-3.0-or-later 开源；它基于 magenie33 的 WFSim（同为 AGPL-3.0-or-later），任务协议与客户端逻辑移植自 WFSim 的网页客户端，这部分版权归 WFSim 作者及贡献者。simfleet 本身不含 WFSim 引擎，运行时从 wfsim.app 下载官方发布的引擎；原生引擎由本仓库的 Action 从对应的 WFSim 提交编译，编译前删去 Digital Extremes 的卡牌文本（WFSim 对这部分没有授权），与同一提交编译的 wasm 逐位比对通过后才发布，simfleet 使用前还会再和网站的 wasm 比对一次。本项目非官方，与 WFSim、Digital Extremes 均无关联。

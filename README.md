# FlowRule (流律)

> 按进程限速的 Windows 网络带宽管理工具。
> 基于 WinDivert 内核级数据包捕获 + Rust / Tauri 2 桌面应用。

FlowRule 运行在**网络层**(WinDivert network layer),能捕获本机所有进出网络流量,通过五元组流表把每个数据包归属到具体进程,再按进程(或"其他流量"兜底)应用独立的下载/上传速率限制。限速是**内核级**的——不依赖目标程序自身实现,任何程序(Steam、浏览器、下载器、游戏)都无法绕过。

界面采用 Miuix 风格玻璃拟态设计:进程表格实时显示每个程序的上下行速率,点击列头可按下载/上传降序排序,点击「限速」即可为该程序设置独立带宽或完全阻断。

## 特性

- **按进程限速**:每个程序独立的上行/下行带宽限制,互不影响
- **进程表格 UI**:实时速率、累计流量、规则状态一目了然;点击「↓ 下载 / ↑ 上传」列头按当前速率降序排列
- **完全阻断**:一键切断指定程序的全部网络流量(比限速更狠)
- **其他流量兜底**:除已设规则的程序外,其余流量可统一限速
- **总开关**:master 开关一键停用所有限速,立即恢复满速(限速规则仍保留)
- **60 秒实时曲线**:总上行/总下行速率图,KB/MB 单位切换
- **引擎诊断栏**:收包/转发/丢弃/进程识别率与四张内核 socket 表计数,便于排查
- **明暗双主题**:跟随系统或手动切换,Miuix 玻璃质感
- **单实例运行**:重复启动自动聚焦已有窗口
- **规则持久化**:限速规则、总开关与兜底限制保存在 `%APPDATA%\rl-app\rules.json`,重启软件后自动恢复

## 架构

```
┌─────────────────────────────────────────────┐
│  FlowRule.exe (Tauri 2)                     │
│  ┌───────────────────────────────────────┐  │
│  │  前端 (HTML/CSS/JS, Miuix 风格)        │  │
│  │  · 进程表格 + 实时曲线 + 限速弹窗       │  │
│  │  · 220ms 轮询 get_snapshot             │  │
│  └───────────────────────────────────────┘  │
│  ┌───────────────────────────────────────┐  │
│  │  rl-engine (Rust)                     │  │
│  │  · flow-events:流层事件维护 PID 归属   │  │
│  │  · engine:网络层捕获 + 令牌桶判定      │  │
│  │  · drainer:回填令牌、重注入排队报文    │  │
│  │  · aggregator:1Hz 快照 + 命令 + 持久化 │  │
│  └───────────────────────────────────────┘  │
│  ┌───────────────────────────────────────┐  │
│  │  WinDivert 内核驱动                    │  │
│  │  · 网络层:捕获 + 重注入所有报文        │  │
│  │  · 流层:连接建立事件(带 PID)         │  │
│  └───────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
```

四个后端线程通过 `Arc<Shared>` 无锁纪律协作(每线程同时最多持一把锁),数据包路径不经过任何 UI 代码。

## 目录结构

```
FlowRule/
├── crates/
│   ├── rl-app/          # Tauri 应用:窗口、托盘、命令接口、前端
│   │   └── frontend/    # 内嵌 HTML/CSS/JS 界面(无 npm 依赖)
│   └── rl-engine/       # 限速引擎核心
│       └── src/backend/ # engine / flows / sockets / rules / synthetic
├── assets/              # WinDivert 驱动与库文件
├── .cargo/config.toml   # 构建配置(WINDIVERT_PATH)
└── Cargo.toml           # workspace 根
```

## 构建

### 前置要求

- Windows 10/11 x64
- [Rust 工具链](https://rustup.rs/)(stable)
- Visual Studio Build Tools(C++ 桌面开发,用于链接)
- [Tauri 2 依赖](https://tauri.app/start/prerequisites/)(WebView2 运行时,Win11 自带)
- WinDivert 2.2.2-A 开发文件(`WinDivert.lib` / `WinDivert.dll` / `WinDivert64.sys`),放在任意目录并在 `.cargo/config.toml` 的 `WINDIVERT_PATH` 中指向它

### 编译

```bash
git clone https://github.com/Gpan666/FlowRule.git
cd FlowRule
cargo build --release -p rl-app
# 产物:target/release/FlowRule.exe
```

### 运行

把 `target/release/FlowRule.exe` 与 `assets/` 下的 `WinDivert.dll`、`WinDivert64.sys`、`WebView2Loader.dll` 放在同一目录,以**管理员身份**运行。首次启动时 WinDivert 驱动会自动加载(需管理员权限)。

> 驱动服务残留排查:若启动报「参数错误」,以管理员运行 `sc query windivert` 确认服务状态,必要时 `sc delete windivert` 后重启电脑再试。

## 限速原理

1. **捕获**:网络层 WinDivert 句柄(`!loopback` 过滤)捕获本机所有进出报文;捕获与重注入共用同一句柄,避免低优先级句柄丢弃重注入包。
2. **归属**:流层事件句柄记录每个连接建立时的 PID;引擎按五元组查流表归属报文,查不到时用内核 socket 表快照(每 2s 刷新,含 IPv4/IPv6 的 TCP/UDP 四张表)兜底合并。
3. **判定**:命中规则则查该程序对应方向的令牌桶;无规则则走「其他流量」兜底限制;总开关关闭时全部直通。
4. **执行**:令牌不足时报文进入该桶队列,由 drainer 线程每 5ms 回填令牌后重注入;完全阻断则直接丢弃。TCP 靠丢包/延迟自然产生背压,UDP 按队列上限(256 包 / 1MB)丢弃。
5. **速率统计**:聚合线程每秒按进程汇总字节数,产出快照供 UI 曲线与表格使用。

令牌桶突发容量为 200ms 速率量(下限 8KB),限速生效后约 0.2~0.5 秒内压到位,稳态按设定速率匀速放行。

## 已知限制

- IPv6 流量已完整支持(TCP/UDP 归属均正常);仅 IPv6 无状态地址频繁轮换的极端场景下,流表可能短暂未命中,由 socket 表兜底
- 限速针对本机网卡带宽,局域网内网流量同样会被统计和限制(loopback 除外)
- 驱动为 WinDivert 官方签名版本,Windows 11 24H2+ 的 HVCI/驱动签名策略变化可能导致加载失败(当前 26H2 实测正常)

## 贡献者

<a href="https://github.com/Gpan666"><img src="https://img.shields.io/badge/TSBYJ-作者-3482FF?style=flat-square" alt="TSBYJ"></a>
<a href="https://www.trae.cn"><img src="https://img.shields.io/badge/TRAE-AI%20IDE-6E56CF?style=flat-square" alt="TRAE"></a>
<a href="https://www.trae.cn"><img src="https://img.shields.io/badge/WorkBuddy-AI%20助手-00B3A7?style=flat-square" alt="WorkBuddy"></a>

## English

**FlowRule** is a per-process network bandwidth limiter for Windows, powered by WinDivert kernel-level packet capture and built with Rust + Tauri 2.

It captures every inbound/outbound packet at the network layer, attributes each packet to a process via a flow table (plus kernel socket-table snapshots as fallback), and applies independent download/upload speed limits per process — with a global master switch, an "other traffic" fallback limit, full block, live per-process rate table with sortable columns, a 60-second total-rate chart, and an engine diagnostics bar.

Rules persist to `%APPDATA%\rl-app\rules.json` and survive restarts.

### Build

```bash
git clone https://github.com/Gpan666/FlowRule.git
cd FlowRule
cargo build --release -p rl-app
```

Requires Rust stable, Visual Studio Build Tools (C++ desktop), WinDivert 2.2.2-A dev files (point `WINDIVERT_PATH` in `.cargo/config.toml` at them), and the WebView2 runtime.

### Run

Place `FlowRule.exe` together with `WinDivert.dll`, `WinDivert64.sys` and `WebView2Loader.dll`, then run **as administrator**. The WinDivert driver loads automatically on first run.

### How it works

- A single network-layer WinDivert handle captures all non-loopback traffic; capture and re-injection share this handle to avoid priority-related packet loss.
- A flow-layer handle records connection-establishment events with their PID; the engine maps every packet to a process by its 5-tuple.
- Per-`(process, direction)` token buckets (200 ms burst) meter the traffic; packets without enough tokens are queued and re-injected by a drainer thread every 5 ms, creating natural TCP backpressure. Blocked processes have packets dropped outright.
- An aggregator thread emits a 1 Hz snapshot (per-process rates, totals, history) to the UI and persists rule changes.

## License

MIT

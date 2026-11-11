# 流律 (liulv)

> 一个 **Windows 进程级网络带宽限速工具**。为每个程序单独设置上 / 下行速率上限，实时观测流量曲线。
> 后端基于 Rust + [WinDivert](https://www.reqrypt.org/windivert.html) 限速引擎，前端采用 **Tauri 2 + Miuix (HyperOS / MIUI)** 设计语言。

[English](#english) · 许可证：[MIT](#许可证)

---

## ✨ 功能特性

- **进程级限速**：自动发现运行中的程序（名称 + 多进程），为每个程序单独设置下载 / 上传速率上限
- **全局总开关**：一键开启 / 暂停限速，暂停后瞬时恢复满速
- **多种预设挡位**：极慢 / 慢速 / 标准 / 快 / 很快 / 1 兆 / 2 兆 / 5 兆 / 不限，一键切换即时生效
- **自定义速率**：支持任意数值输入，单位 **KB/s** 与 **MB/s** 可切换，并显示 **Mbps** 参考值消除歧义
- **双向独立**：可只限下行、只限上行、或双向分别设置
- **其他流量独立可控**：未被单独限速的流量默认「按原速放行」（不限速）；如需全局限速，可在「其他流量」卡片主动设置
- **实时观测**：超大数字速率显示 + 近 60 秒平滑面积曲线，肉眼验证是否生效
- **真实图标**：列表与详情显示程序真实 exe 图标，无图标的程序只显示名称
- **Miuix 风格界面**：Squircle 大圆角卡片、毛玻璃层次、胶囊开关、分段控件、明暗双主题

## 📸 截图

| 浅色主题 | 深色主题 |
| --- | --- |
| ![浅色主题](docs/screenshots/ui-light.png) | ![深色主题](docs/screenshots/ui-dark.png) |

## 🏗️ 架构

```
┌──────────────────────────────────────────────────┐
│  Tauri 2 前端（HTML / CSS / Miuix 设计系统）        │
│  自绘标题栏 · 卡片布局 · 明暗主题 · 零 npm 依赖       │
└──────────────────▲─────────────────┬──────────────┘
                   │ Tauri invoke    │ Tauri events
┌──────────────────┴─────────────────▼──────────────┐
│  rl-app（Rust 后端）                                │
│  共享状态 Arc<Mutex>（带超时加锁）· 单后台线程       │
│  配置落盘 config/settings.json                     │
└─────────┬──────────────────────────┬───────────────┘
          │                          │
┌─────────▼──────────┐   ┌───────────▼──────────────┐
│  rl-core            │   │  rl-engine-win            │
│  令牌桶 · 单位换算   │   │  WinDivert 2.2 全局句柄×2 │
│  挡位 · 统计采样     │   │  五元组→PID→令牌桶分派     │
└────────────────────┘   └───────────────────────────┘
```

- **rl-core**：纯逻辑，无平台依赖，可单元测试（单位换算、令牌桶、预设挡位、速率统计）
- **rl-engine-win**：WinDivert 2.2 限速引擎 + `GetExtendedTcpTable/UdpTable` 进程归属映射，
  用户态截包 → 五元组识别进程 → 分派令牌桶 → 重注入 / 丢弃
- **rl-app**：Tauri 2 后端（共享状态 + 单后台线程负责引擎 poll / 进程扫描 / 采样）与 Miuix 前端

## 📁 目录结构

```
liulv-ratelimiter/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ rl-core/                # 纯逻辑：单位 / 令牌桶 / 挡位 / 统计
│  ├─ rl-engine-win/src/
│  │  ├─ lib.rs               # 全局句柄 + 五元组分派 + 令牌桶
│  │  └─ proc_map.rs          # 连接表→PID 映射（iphlpapi.dll）
│  └─ rl-app/                 # Tauri 2 后端 + Miuix 前端
│     ├─ frontend/            # index.html / styles.css / app.js（零 npm 依赖）
│     ├─ icons/               # 应用图标
│     ├─ capabilities/        # Tauri 权限
│     └─ src/                 # main.rs + icons.rs + settings.rs
├─ assets/                    # WinDivert.dll / WinDivert64.sys / WebView2Loader.dll
├─ tools/                     # check_filter.py / verify_no_freeze.mjs / verify_ui.py 等
├─ docs/screenshots/          # README 用截图
├─ PLAN.md                    # 开发规划与决策记录
└─ README.md
```

## 🔧 构建

### 前置条件

- Rust 工具链（本项目使用 `stable-x86_64-pc-windows-gnu`；MSVC 也可）
- Windows 10 / 11（**需管理员权限**以加载 WinDivert 驱动）
- WebView2 Runtime（Windows 11 自带；Windows 10 需安装）
- 本机 Git 凭据管理器（GCM）已登录 GitHub

```bash
# 克隆
git clone https://github.com/Gpan666/liulv-ratelimiter.git
cd liulv-ratelimiter

# 构建（GNU 工具链）
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --workspace

# 运行（需管理员）
# 构建产物在本机 rl-target/release/rl-app.exe
# 把 dist/ 下的 WinDivert.dll / WinDivert64.sys / WebView2Loader.dll 与 exe 放同目录
# 右键「以管理员身份运行」rl-app.exe
```

> 注：本项目通过 `.cargo/config.toml` 把编译输出目录重定向到纯英文路径，
> 因为 GNU 工具链的 `ld` 无法处理含中文的项目目录。

### CNB 自动构建（.cnb.yml）

仓库根目录的 `.cnb.yml` 配置了 CNB 云原生构建流水线：

| 触发 | 行为 |
|---|---|
| PR 合并进 `main`（`pull_request.merged`） | 在 Windows 构建节点编译 exe，打包 zip 作为**提交附件** |
| 直接推 `main`（`push`） | 同上 |
| 推送 `v*` Tag（`tag_push`） | 编译 exe，**创建 Release** 并附上 zip |

构建走**自托管 Windows 节点**（CNB 公共节点全是 Linux，无法直接产出 Windows exe）：

1. 根组织「tsbyj.666 → 组织设置 → 构建节点」接入一台 Windows 机器，标签至少包含 `windows`
2. 构建机需准备：Rust（`stable-x86_64-pc-windows-msvc`）、Visual Studio Build Tools
   （工作负载「使用 C++ 的桌面开发」，提供 `link.exe`）、git
3. 流水线脚本：`tools/ci/build-win.ps1`（构建打包）、`tools/ci/upload-artifact.ps1`（上传附件）

> 自托管节点默认不开启 Docker，因此附件上传直接走 CNB OpenAPI，而不是 `cnbcool/attachments` 插件。
> 脚本会把源码用 `git archive` 导出到 `%TEMP%` 下的纯 ASCII 目录再编译，规避中文路径导致的链接失败。

## 🚀 运行

1. 确保 `rl-app.exe` 与以下文件**同目录**：
   - `WinDivert.dll`、`WinDivert64.sys`（限速驱动，MIT 许可，官方签名）
   - `WebView2Loader.dll`（GNU 工具链下必须随包，否则启动报 `0xC0000135`）
2. **双击 `rl-app.exe` 即可**：普通启动不会强制弹 UAC。若当前不是管理员，窗口顶部会出现橙色「需要管理员权限」横幅，点击「以管理员身份运行」按钮即可**自提权重启**（用 `ShellExecute runas`）；已是管理员则横幅不显示，直接可用。
3. 在左侧选择程序 → 右侧设置上 / 下行限速挡位或自定义速率
   - 预设挡位最高只到 **5 MB/s**；要设更大的值（如 20 MB/s）请点末端的 **「自定义」** 按钮，
     在出现的输入框里直接输入数字（单位随顶部的 KB/s · MB/s 切换）。
     输入后**立即按该数值生效**，不会再退回默认挡位。
4. **点挡位或方向开关即自动开启限速**（无需再手动开总开关）；顶部总开关用于全局暂停。
   ⚠️ 总开关本身**不会**自动限速任何流量——必须选程序或主动设置「其他流量」才会真正接管。
5. 未单独限速、且未在「其他流量」里设置的程序，**原速放行、不被限速**
6. 单位、限速规则、总开关等设置会自动保存（同目录 `config/settings.json`），下次启动恢复
7. 总览卡片右下方有一行**引擎实时诊断**（句柄数 / 桶数 / 收包 / 命中限速 / 丢弃 /
   程序识别 / 去重 / 连接表规模 / 生效桶明细）。若「句柄 0/2」说明未接管流量；「丢弃」计数在涨说明限速正在生效；
   **「收包」在涨但「程序识别」「命中限速」一直是 0 → 这套网络下程序归属识别失效（典型是 VPN / 加密代理中转），
   对应程序的限速不可能生效**，界面会直接给出警告；
   「生效桶」里若出现 `其他流量 下行 = …`，说明你手动开启了全局限速（会限制**所有**未被单独限速的流量）；
   诊断行末尾的 `v6 a/b` 是 IPv6 连接表（TCP/UDP）条目数
8. ⚠️ **从 v0.1.0 升级**：旧版曾自动给「其他流量」套 1 MB/s 默认挡位，导致「单限某个程序却整台电脑被限速」。
   新版启动时会**自动清除**这个遗留默认值（你手动输入过自定义速率的「其他流量」设置会被保留）。
   若升级后你确实想全局限速，在「其他流量」卡片重新设置一次即可。

## ⚙️ 限速原理

WinDivert 在网络栈拦截 IP 包，引擎在**用户态**识别包属于哪个程序并分派令牌桶：

```
网卡 → WinDivert 全局句柄(上/下行各 1 个)
     → 解析 IPv4 / IPv6 + TCP/UDP 五元组
     → GetExtendedTcpTable/UdpTable 查 owning PID
     → PID→程序规则→令牌桶 → 放行(reinject) / 丢弃
```

- **为什么不用 `processId` 过滤**：WinDivert 2.2 的 `WINDIVERT_LAYER_NETWORK` 明确不支持
  `processId` 字段（`processId` 仅在 FLOW / SOCKET / REFLECT 层可用），带它的表达式会
  让 `WinDivertOpen` 返回错误 87。因此改用连接表在用户态做归属判定。
  `tools/check_filter.py` 会用真实 DLL 离线编译校验所有 filter，防止回归。
- 上 / 下行各一个全局句柄，filter 天然互斥，不存在「多句柄重叠重复注入」问题
- 超过速率的包被丢弃，TCP 拥塞控制会自动降速（下行限速的「正常机制」）
- 令牌上限 = max(速率 × 100ms, 16 个 MTU) + 一个时间片进账，且补桶只算最近 100ms：
  空闲再久也不会一次性补满整段空闲的额度（避免「停顿后突发」绕过限速），
  同时保证一串 TCP 突发能整串放行（额度太小会丢包失控、TCP 卡死）
- **filter 里不含任何网卡条件**（既没有 `not loopback`，也没有 `not ifIdx != 1`）。
  句柄必须看到本机的**全部** TCP/UDP 流量 —— 按网卡猜一次，猜错的方向就是
  「流量根本不进句柄」，用户看到的是收包数极小、命中限速 0、丢弃 0，设多少都像没限。
  「同一个包只限一次」改由引擎在**用户态按包内识别码去重**（IPv4 的 identification、
  IPv6 的流标签，200ms 窗口，上下行各一套）：VPN / 代理中转时同一份数据会在虚拟网卡与
  物理网卡各出现一次，重复副本直接放行、不重复扣令牌
- 连接表（`MIB_*TABLE_OWNER_PID`）IPv4 表头是 `dwNumEntries` + 4 字节对齐填充 = 8 字节，
  行大小 24（TCP）/ 12（UDP）；偏移少读 4 字节填充就会让每条记录整体错位，
  PID 全部读成 0、所有流量误判成「其他流量」
- 引擎每轮**按方向排空**驱动队列（预算内、0 超时立即取），不再「两方向各取 1 个包」，
  否则下行会被上行的零星 ACK 拖慢
- `not impostor` 排除重注入的包，避免自捕获死循环；连接表每 700ms 刷新一次
- 限速对象为「程序」（同一 exe 的多进程共用一个带宽预算），跨 Windows 10 / 11 通用

## 📌 已知限制

- 仅支持 Windows（依赖 WinDivert），且需管理员权限加载驱动
- 只对 **TCP/UDP** 生效（IPv4 + IPv6 均接管）；ICMP 及非首片 IP 分片不走程序归属判定（归入「其他流量」）
- 程序级归属：IPv4 的 TCP/UDP 全部支持；**IPv6 仅 UDP 可归属**（即 QUIC / HTTP3 可以按程序限速），
  IPv6 的 TCP 因 Windows `GetExtendedTcpTable` 不支持 `AF_INET6`（恒返回 Win32 87）暂时归入「其他流量」
- 进程归属依赖系统连接表，连接刚建立 / 已关闭的短暂窗口内可能归入「其他流量」
- 若流量全程经过**加密代理中转**（Clash TUN / v2rayN / WireGuard 等），系统连接表里只能看到
  代理进程，看不到真实发起程序。此时程序级限速无法命中，诊断行会显示「程序识别」长期为 0
  并给出警告；可改用「其他流量」全局限速，或让代理软件把该程序设为直连
- 未做系统托盘、开机自启、安装包（见 `PLAN.md` 里程碑）
- 高带宽（千兆）下用户态逐包处理 CPU 占用会上升

## 📜 许可证

本项目以 **MIT 许可证** 开源。

- 限速引擎依赖 [WinDivert](https://www.reqrypt.org/windivert.html)（MIT，官方签名驱动）
- 前端设计语言参考 Xiaomi **Miuix / HyperOS**

---

## English

**liulv (流律)** is a Windows per-process network bandwidth limiter. It sets separate download/upload speed caps for each running program and shows a live traffic graph. The backend is Rust + [WinDivert](https://www.reqrypt.org/windivert.html); the frontend is **Tauri 2** styled with the **Miuix (HyperOS/MIUI)** design language.

### Features
- Per-process download/upload rate limiting (auto-discovers running programs)
- Global master on/off switch; instant restore to full speed when paused
- Preset speed tiers (极慢 → 5 兆) plus custom rates; KB/s ⇄ MB/s with Mbps reference
- Independent up/down limits; "other traffic" catch-all bucket
- Big-number live readout + 60-second smooth area chart
- Real exe icons; Miuix-style cards, glass layers, capsule toggles, light/dark themes

### Build & Run
```bash
git clone https://github.com/Gpan666/liulv-ratelimiter.git
cd liulv-ratelimiter
cargo build --release --workspace
# Run rl-app.exe as Administrator, alongside WinDivert + WebView2Loader DLLs from dist/
```

Requires Administrator to load the WinDivert driver. Windows only.

Licensed under MIT.

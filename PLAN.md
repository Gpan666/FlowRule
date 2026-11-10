# 限速软件（RateLimiter）开发规划

> 目标：一个**原生 Rust GUI** 的 Windows 带宽限速工具。功能收敛在「限速」这一件事上：
> 选单位（KB/s / MB/s）→ 选挡位 → 生效。不套浏览器壳（不用 Tauri / Electron），不用 WebView。

---

## 1. 需求边界

### 1.1 必须做（MVP）
| 编号 | 需求 | 说明 |
|---|---|---|
| M1 | 单位选择 | KB/s 与 MB/s 可切换（内部统一按字节计），界面上也可看 Kbps/Mbps 参考值 |
| M2 | 内置挡位 | 一组预设挡位按钮，一键切换，切换即时生效（<200ms） |
| M3 | 自定义速率 | 输入框 + 单位下拉，可填任意速率 |
| M4 | 上行/下行分别限速 | 可只限下行、只限上行、或双向分别设 |
| M5 | 总开关 | 一键开启/暂停限速，暂停后立刻恢复满速 |
| M6 | 实时速率显示 | 当前上/下行速率 + 近 60 秒曲线，能肉眼验证是否生效 |
| **M7** | **发现运行中的程序** | 自动枚举进程（名称 + PID，3 秒刷新），支持按名称/PID 搜索 |
| **M8** | **按进程单独限速** | 选中进程后单独设上/下行速率；未被单独限速的流量走「其他流量」兜底 |

### 1.2 暂不做（v1 之后再说）
按端口/IP 限速、规则持久化到配置文件、进程图标、开机自启、系统托盘、
流量配额与定时策略、macOS/Linux 支持。

### 1.3 明确不做
代理/VPN 功能、广告过滤、抓包分析、账号体系、联网更新。

---

## 2. 技术选型

### 2.1 限速引擎：WinDivert（推荐，主方案）

WinDivert 是 Windows 上的用户态抓包/转发驱动（MIT 许可，官方签名驱动，可随包分发）。
工作方式：在网络栈中拦包 → 我们自己决定「放行（重注入）」还是「丢弃」。

```
网卡 → WinDivert 拦截 → 令牌桶判定 → 放行(reinject) / 丢弃
```

- **接入方式**：运行时用 `libloading` 动态加载 `WinDivert.dll`，不链接 MSVC 的 `.lib`。
  好处：不依赖 MSVC 生成工具、GNU 工具链也能编译、缺驱动时优雅降级而不是崩溃。
  （备选是 `windivert` crate，但它走编译期链接，需要 MSVC `.lib`。）
- **权限**：需要管理员权限加载驱动（随包放 `WinDivert.dll` + `WinDivert64.sys`）。
- **优点**：系统级真限速、无需改路由、可按进程/端口过滤、跨版本 Windows 10/11 通用。
- **代价**：丢包式限速对 TCP 有效（触发拥塞控制降速）但会有轻微重传抖动；逐包在用户态处理，带宽很高时 CPU 占用会上升。

### 2.2 备选方案对比（供决策）

| 方案 | 系统级 | 需管理员 | 需外部驱动 | 复杂度 | 平滑度 | 结论 |
|---|---|---|---|---|---|---|
| **A. WinDivert 抓包限速** | ✅ | ✅ | ✅ WinDivert | 中 | 中（丢包抖动） | **推荐 MVP** |
| B. Wintun 虚拟网卡 + smoltcp | ✅ | ✅ | ✅ Wintun | 高 | 高（排队整形） | v2 演进方向 |
| C. 本机 SOCKS/HTTP 代理限速 | ❌ 仅代理流量 | ❌ | ❌ | 低 | 高 | 太受限，不选 |
| D. 系统 QoS 策略（NetQosPolicy） | ✅ | ✅ | ❌ | 低 | 低 | 家庭版受限、精度差，不选 |

> 决策：走方案 A。但把引擎抽象成 `trait Engine`，B 方案日后可作为第二个实现插进来，GUI 不改。

### 2.3 GUI：egui / eframe（原生渲染，非浏览器）

- `eframe` + `egui`：立即模式 GUI，用 wgpu/OpenGL 直接绘制，Rust 原生，单二进制，无 WebView、无 Node、无 Chromium。
- 体积小（release 约 8–15MB）、启动快、跨平台代码结构一致。
- 备选：`slint`（声明式、更精致但定制成本高）、`iced`。
- 中文显示：egui 需加载字体（如思源黑体 / 微软雅黑）并设置 `FontDefinitions`。

---

## 3. 架构设计

### 3.1 Workspace 结构

```
限速软件/
├─ Cargo.toml              # workspace
├─ crates/
│  ├─ rl-core/             # 纯逻辑，无平台依赖，可单元测试
│  │   ├─ unit.rs          # 单位与换算
│  │   ├─ bucket.rs        # 令牌桶
│  │   ├─ profile.rs       # 挡位定义
│  │   └─ stats.rs         # 速率统计/采样环形缓冲
│  ├─ rl-engine-win/       # WinDivert 引擎实现
│  │   ├─ divert.rs        # 句柄、filter、收发循环
│  │   └─ mod.rs           # impl Engine for WinDivertEngine
│  └─ rl-app/              # egui GUI
│      ├─ main.rs
│      └─ ui/              # 挡位条、单位选择、速率曲线、开关
├─ assets/                 # WinDivert.dll / WinDivert64.sys / 字体
└─ PLAN.md
```

### 3.2 线程模型

```
┌──────────── 主线程 (eframe) ────────────┐
│  UI 渲染，每帧读取共享状态               │
└─────────────────────────────────────────┘
        ▲ 统计(Arc<RwLock<Stats>>)   │ 命令(crossbeam channel)
        │                            ▼
┌──────────── 引擎线程 ───────────────────┐
│  WinDivert recv 循环                     │
│  → 令牌桶判定 → reinject / drop          │
│  → 累加字节数 → 每 100ms 落一次采样      │
└─────────────────────────────────────────┘
```

- 命令：`Start / Stop / SetRate{dir, bytes_per_sec} / Shutdown`，单向 channel。
- 统计：`AtomicU64` 累加字节；每 100ms 计算瞬时速率并 push 到 600 点环形缓冲（约 60 秒曲线）。
- 引擎线程与 GUI 完全解耦，GUI 卡顿不会丢包（WinDivert 自带内核队列）。

### 3.3 核心数据结构（rl-core）

```rust
#[derive(Clone, Copy)]
pub enum Unit { Kb, Mb }          // 界面单位（按字节计：KB/s、MB/s）

#[derive(Clone, Copy)]
pub struct Rate { pub value: f64, pub unit: Unit }

impl Rate {
    pub fn bytes_per_sec(self) -> u64 {          // 内部唯一真值
        match self.unit {
            Unit::Kb => (self.value * 1024.0) as u64,
            Unit::Mb => (self.value * 1024.0 * 1024.0) as u64,
        }
    }
    pub fn bits_per_sec(self) -> u64 { self.bytes_per_sec() * 8 }
}

pub struct TokenBucket {
    rate_bps: u64,        // 字节/秒
    capacity: u64,        // 桶容量 = rate * 0.1s（允许 100ms 突发）
    tokens: f64,
    last: Instant,        // 单调时钟
}

impl TokenBucket {
    /// 返回 true 表示允许该包通过（扣减 tokens）
    pub fn try_consume(&mut self, bytes: u64) -> bool;
    pub fn set_rate(&mut self, rate_bps: u64);
}

pub struct Profile { pub id: u32, pub name: &'static str, pub rate: Rate } // 预设挡位
pub struct Stats  { pub up_bps: u64, pub down_bps: u64, pub history: RingBuffer<f32, 600> }
```

### 3.4 预设挡位表

| 挡位 | 显示（自动随单位换算） |
|---|---|
| 极慢 | 32 KB/s |
| 慢速 | 64 KB/s |
| 标准 | 128 KB/s |
| 快 | 256 KB/s |
| 很快 | 512 KB/s |
| 1 兆 | 1 MB/s（= 1024 KB/s） |
| 2 兆 | 2 MB/s |
| 5 兆 | 5 MB/s |
| 不限 | 关闭该方向限速 |

挡位按钮在「KB/s」单位下显示为 `32/64/128/256/512/1024/2048/5120 KB/s`，
切到「MB/s」则显示 `0.031/0.063/…/1/2/5 MB/s`，同一组挡位只是展示换算，不需要两套数据。

---

## 4. 关键实现细节

### 4.1 WinDivert 收包循环

```rust
// 下行：拦截收到的包  上行：拦截发出的包
let filter = "inbound and ip and (tcp or udp)";   // 下行句柄
let mut w = WinDivert::new(filter, Layer::Network, 0, 0)?;

loop {
    // 每 64 个包取一次时间，摊薄时钟开销
    let pkt = w.recv()?;                       // 阻塞收包
    let len = pkt.data().len() as u64;
    if bucket.try_consume(len) {
        w.send(&pkt)?;                         // 重注入，校验和由驱动处理
        stats.add(len);
    }                                          // 否则丢弃：TCP 触发拥塞控制降速
}
```

- 上行/下行各开一个句柄、两个令牌桶，可独立限速。
- WinDivert 重注入的包不会被自己再捕获，无环路风险。
- filter 里加 `and not loopback` 避免本机回环流量干扰。

### 4.2 抖动优化（v0.2 加）
纯丢包会让 TCP 速率锯齿明显。改进：超限的包不丢，放进 `VecDeque` 延迟队列，
下次 refill 后再 reinject；队列满（如 256 个包 / 200ms）才真丢。平滑度显著提升。

### 4.3 权限与打包
- **不**嵌入 `requireAdministrator` manifest（实测会强制 UAC 弹窗、卡住非管理员账户、并让 `cargo test`
  因 os error 740 无法运行）。改为**运行时检测 + 横幅自提权**：
  - 后端用 `IsUserAnAdmin()` 判定，snapshot 带 `elevated` 字段下发给前端；
  - 非管理员时前端顶部显示橙色「需要管理员权限」横幅 + 「以管理员身份运行」按钮；
  - 点击按钮调用 `relaunch_admin` 命令，用 `powershell Start-Process -Verb RunAs` 以管理员身份重启自身后退出。
- 发布产物：`rl-app.exe` + `WinDivert.dll` + `WinDivert64.sys` + `WebView2Loader.dll`，可选 Inno Setup 打成安装包。
- 单文件偏好：可用自解压或把 dll/sys 释放到 `%LOCALAPPDATA%` 目录（WinDivert 支持自定义驱动路径）。

### 4.4 中文字体
`egui::FontDefinitions` 中插入 `assets/SourceHanSansSC-Regular.otf`（或系统微软雅黑 `C:/Windows/Fonts/msyh.ttc`），
设置 `FontFamily::Proportional` 优先，避免方块字。

---

## 5. 里程碑

**当前进度（2026-10-15）**：v0.1–v0.3、v0.6（进程发现与按进程限速）已完成并通过编译，
24 个单测全绿，GUI 可启动。v0.4（速率曲线）、v0.5（提权 manifest、托盘、安装包）待做。

### 进程限速的关键约束

WinDivert 多个句柄的 filter **不能重叠**，否则同一个包会被多个句柄各重注入一次（表现为网速异常、
连接错乱）。所以：

- 某个进程被单独限速 → 句柄 filter 为 `... and processId == N`
- 「其他流量」的句柄 filter 为 `... and processId != N1 and processId != N2 ...`（排除所有已限速进程）
- 进程规则增删时，「其他流量」的 filter 变了 → **必须重开句柄**（引擎里按 filter 字符串 diff）
- 排除项上限 24 个（`MAX_EXCLUDES`），超过则无法再对「其他流量」限速，会给出明确错误

| 阶段 | 内容 | 交付物 | 验收标准 |
|---|---|---|---|
| **v0.1 骨架** | workspace、rl-core 单测、空窗口 | 能跑起来的原生窗口 | `cargo test` 全绿；窗口启动 <1s |
| **v0.2 引擎** | WinDivert 收包 + 令牌桶 + CLI 验证 | `rl-cli --down 1MB/s` | iperf3/网盘下载实测速率稳定在设定值 ±10% |
| **v0.3 界面** | 单位切换、挡位条、自定义输入、总开关 | 完整 GUI | 切挡位 200ms 内生效；暂停后 1s 内恢复满速 |
| **v0.4 观测** | 实时速率数字 + 60 秒曲线 + 状态提示 | 可视化 | 曲线与任务管理器/资源监视器数值一致 |
| **v0.5 打磨** | 管理员提权、错误处理、开机自启、托盘、发布包 | 安装包 | 全新机器双击安装即可用 |

---

## 6. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| 安全软件拦截 WinDivert 驱动 | 完全不可用 | 使用官方签名驱动；启动失败时明确提示「被安全软件拦截，请加白名单」 |
| 丢包式限速抖动 | 速率不稳 | v0.2 引入延迟队列整形；桶容量给 100ms 突发 |
| 高带宽 CPU 占用 | 千兆下 CPU 高 | 批量取时、必要时只对下行做整形；后续换 Wintun 方案 |
| 下行限速"浪费"带宽 | 包已到网卡才丢 | 文档说明：下行靠丢包压制，属正常机制 |
| VPN/虚拟机网卡干扰 | 限速对象不对 | MVP 先全网卡生效，v1 支持选网卡 |
| 单位歧义（Kb 比特 vs KB 字节） | 用户设错 8 倍 | 界面同时显示 `1 MB/s = 1024 KB/s = 8 Mbps`，消除歧义 |

---

## 7. 测试计划

**单元测试（rl-core）**
- 单位换算：`1 MB/s == 1024 KB/s == 8388608 bps`
- 令牌桶：无突发时 1s 内恰好放行 `rate` 字节；`set_rate` 后立刻按新速率
- 环形缓冲：写入 1000 点后仍只保留最近 600 点

**手工验证清单**
1. 设 1 MB/s 下行 → 下载大文件，稳定在 ~1 MB/s
2. 单位切 MB/s，输入 0.5 → 实测 ~512 KB/s
3. 只限上行 128 KB/s → 上传大文件被压制，下载不受影响
4. 点「暂停」→ 立即恢复满速
5. 关闭程序 → 网络完全恢复，无残留（驱动卸载）
6. 非管理员启动 → 正确弹提权提示

---

## 8. 已确认的决策（2026-10-15）

| 决策点 | 结论 |
|---|---|
| 限速范围 | **首版只做全局**（所有上行/下行流量），按进程限速推迟到 v1 |
| 限速引擎 | **WinDivert**，接受随包附带官方签名驱动（管理员权限） |
| 单位口径 | 主单位 **KB/s、MB/s**（按字节），界面附带 **Mbps** 换算值 |

单位换算基准（内部唯一真值为 `bytes/sec`）：

```
1 KB/s = 1024 B/s        1 MB/s = 1024 KB/s = 1 048 576 B/s
1 KB/s = 8 Kbps          1 MB/s = 8 Mbps
```

## 9. 环境准备（已完成，2026-10-15）

```bash
# 1) 安装 rustup（已装：Rust 1.99.0）
rustup-init.exe -y --default-toolchain stable-x86_64-pc-windows-msvc --profile minimal

# 2) 本机没有 MSVC 生成工具，且当前账户非管理员（装 VS Build Tools 需 UAC 提权），
#    改用免管理员的 GNU 工具链：rustup 自带 rust-mingw 提供链接器
rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal
rustup component add rust-mingw --toolchain stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu
```

### 四个必踩的坑（全部已解决，记录以便换机器复现）

1. **MSVC 工具链没有链接器**：装了 `stable-x86_64-pc-windows-msvc` 但没有 VS 生成工具时，
   编译会报 `linking with link.exe failed`。要么装 VS Build Tools（需管理员），
   要么换 GNU 工具链（本项目已换）。
2. **路径不能含中文**：GNU 的 `ld` 处理不了非 ASCII 路径，项目目录叫「限速软件」会报
   `ld: cannot find ....rcgu.o`。解法是把编译输出重定向到纯英文目录，
   已写进 `.cargo/config.toml`：`target-dir = "C:/Users/MOSS/rl-target"`。
3. **缺 `dlltool.exe` 与 `as.exe`**：`windows-result` / `windows-strings` 用 raw-dylib，
   构建时要用 dlltool 生成导入库，而 dlltool 内部还要调 GNU 汇编器 as。
   rustup 只自带 dlltool（在 `.../bin/self-contained/`），没有 as。
   解法：从 MSYS2 镜像取 `mingw-w64-x86_64-binutils`（清华镜像：
   `https://mirrors.tuna.tsinghua.edu.cn/msys2/mingw/mingw64/`），
   把 `as.exe / dlltool.exe / ar.exe / ranlib.exe / windres.exe` 放进 `~/.cargo/bin`（该目录已在 PATH）。
   用 `C:/Windows/System32/tar.exe` 就能解 `.tar.zst`（bsdtar 3.8 支持 zstd）。
4. **缺系统导入库**：链接时报 `ld: cannot find -lshlwapi`——rustup 自带的 MinGW 库不全。
   解法：再取 `mingw-w64-x86_64-crt-git` 包，把里面 `lib/*.a` 全部复制到
   `~/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/lib/self-contained/`。

```bash
export PATH="/c/Users/MOSS/.cargo/bin:$PATH"   # 之后所有 cargo 命令都要它
cargo test -p rl-core        # 17 个单测：单位换算、令牌桶、挡位、统计
cargo test --workspace
cargo build --workspace      # 首次编译较慢（egui 依赖多）
cargo build --release        # 发布版
```

产物在 `C:/Users/MOSS/rl-target/{debug,release}/rl-app.exe`（不是项目目录下的 target）。

## 10. 目录与首个命令

```bash
cd E:/限速软件
cargo new --lib crates/rl-core        # 或按既定结构手工创建
cargo build --release                 # 首次编译较慢（egui 依赖较多）
```

MVP 验证顺序：先 `rl-core` 单测 → 再 WinDivert 引擎（可用最小 CLI 验证）→ 最后接 GUI。

---

## 11. 架构变更（2026-11-10）：GUI 改为 Tauri 2 + Miuix 设计语言

> 用户明确允许修改架构（「不需要还是 rust+egui」），并在三个方案中选定 **Tauri 2 + Miuix CSS**。
> 因此第 2.3 节的 egui 选型与「不用浏览器壳」的硬性约束**一并作废**，本节为准。

### 11.1 为什么
Miuix 是 HyperOS / MIUI 设计语言（大圆角 squircle 卡片、胶囊开关、分段控件、
毛玻璃层次、超大数字层级）。egui 自绘难以还原毛玻璃与连续曲率圆角；本机又没有
JDK/Gradle（Compose Desktop + 官方 miuix）或 Flutter SDK，但 Node 22、WebView2 Runtime
与 Tauri crate 缓存齐全，所以走 Tauri 2 + HTML/CSS 还原度最高、落地最快。

### 11.2 新结构
```
crates/rl-app/
├─ Cargo.toml / build.rs / tauri.conf.json
├─ capabilities/default.json
├─ icons/{icon.ico,icon.png}
├─ frontend/{index.html,styles.css,app.js}   # Miuix 设计系统，零 npm 依赖
└─ src/main.rs                               # Tauri 命令 + 单后台线程
```
- `rl-core` / `rl-engine-win` **一行未改**，33 个单测仍全绿；程序级限速、句柄不重叠、
  「其他流量」按方向排除等全部不变量原样保留。
- 后端把原 egui 的 `App` 状态搬进 `Arc<Mutex<Inner>>`，**单后台线程**负责
  引擎 `poll` + 3 秒进程扫描 + 100ms 采样；前端每 220ms 拉 `get_snapshot`，
  用 `set_master / set_unit / select_program / set_rule / clear_rule / refresh / win_action` 下发意图。
- 前端不持有业务状态；静态 DOM 骨架 + 局部更新，输入框不会因刷新丢焦点。
- 自绘标题栏（`decorations:false` + `data-tauri-drag-region="deep"`），
  最小化 / 最大化 / 关闭走自定义 Rust 命令；明暗双主题存 localStorage。

### 11.3 三个必须记住的构建坑
1. **windres 打不开中文路径**：tauri-build 默认把 `icons/icon.ico` 的绝对路径写进 `.rc`。
   `build.rs` 必须先把 ico 复制到 `OUT_DIR`（纯 ASCII），再用
   `Attributes::new().windows_attributes(WindowsAttributes::new().window_icon_path(...))`
   `try_build`。否则报 `windres: can't open icon file ... Invalid argument`。
2. **`custom-protocol` feature 必须在 app crate 定义并默认启用**：
   `generate_context!` 内部用 `cfg!(not(feature="custom-protocol"))` 决定是否走 devUrl。
   本项目不跑 dev server，不启用会白屏。
3. **GNU 工具链下需要随包带 `WebView2Loader.dll`**：
   wry 的 `WebView2Loader` 在 MSVC 下静态链接，GNU 下 `dlltool` 会生成动态导入，
   缺它启动即 `0xC0000135`。官方 x64 版已放 `assets/WebView2Loader.dll`，
   发布时必须和 exe 同目录（dist 内已有）。
   （另：导入表里的 `api-ms-win-*.dll` 由 Windows 加载器内建解析，**不是**缺件；
   Git Bash 那条 "cannot open shared object file" 是 MSYS 层误报。）

### 11.4 交付形态（更新）
`dist/` = `rl-app.exe` + `WinDivert.dll` + `WinDivert64.sys` + `WebView2Loader.dll`，
需管理员身份运行（未加 requireAdministrator manifest，避免非管理员账户被 UAC 卡住）。
release 仍是 GUI 子系统、内嵌图标，约 4.7MB。
旧 egui 版完整备份在 `legacy/`（`main_egui.rs.bak` / `Cargo_egui.toml.bak` /
`build_egui.rs.bak` / `rl-app-egui.exe`），可随时回滚。

### 11.5 后续修正（2026-11-10 16:15）

- 产品名改为 **「流律」**（流量 + 律令），副标题保留「网络带宽管理」；
  identifier 与 `rl-app.exe` 文件名不变。
- **窗口拖拽修复**：`data-tauri-drag-region` 走 `plugin:window|start_dragging`，
  而 `core:default` **不含** `allow-start-dragging`（tauri build.rs 里默认 `false`），
  导致自绘标题栏完全拖不动。已在 `capabilities/default.json` 显式补
  `core:window:allow-start-dragging` 与 `core:window:allow-internal-toggle-maximize`（双击最大化）。
- **真实 exe 图标**：`src/icons.rs` 用 `SHGetFileInfoW → GetIconInfo → GetDIBits`
  提 32bpp BGRA → RGBA → PNG → base64 随快照下发；`Inner.icon_cache` 每轮最多提取 24 个，
  避免首次快照卡顿；`png` / `base64` 都在既有 lock 里，没引入新版本。
  无 exe 路径的系统进程（如 `[System Process]`）取不到图标 → 按需求**不渲染图标位、
  只显示程序名**，用 `.pname.no-icon` 的 padding 与有图标行保持文字对齐。
- 验证：`tools/verify_ui.py` 模拟鼠标拖动标题栏得到窗口位移 `(120, 60)` → 拖拽生效；
  明暗两套截图确认列表图标与「选中程序」详情图标均正确显示、透明背景干净。

### 11.6 三处功能性修复（2026-11-10 17:00 后）

用户反馈三问题：**① 单位切换后重启失效；② 限速点击后没反应；③ 预设挡位也无效。**
根因与修复：

- **① 单位/规则重启失效 = 零持久化**：之前完全没有落盘逻辑，任何设置退出即丢。
  新增 `crates/rl-app/src/settings.rs`，把 `unit / master / 其他流量 / 各程序规则 / 当前选中`
  序列化到 exe 同目录 `config/settings.json`（便携、不写注册表）；`Inner::new()` 启动时
  `apply_settings()` 加载，`set_master/set_unit/select_program/clear_rule/set_rule` 末尾 `persist()` 落盘。
- **② 限速点击没反应 = 非管理员加载不了 WinDivert**：引擎逻辑本身正确（33 单测全绿、
  Profile 0–8 映射无误），但用户当前以**非管理员**运行，无法加载内核驱动 → 点击毫无效果，
  且之前无任何提示。改为**运行时检测 + 横幅**（见 4.3）：非管理员顶部显示橙色横幅，
  点「以管理员身份运行」自提权重启，重启后驱动可加载、限速即时生效。
- **③ 预设挡位无效**：同 ②，本质是驱动未加载；横幅给出解决入口后，挡位与自定义速率一并可用。

> 排查中还发现一处**仅存在于开发机**的干扰：早前 manifest 测试在注册表
> `HKCU:\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers` 写入了
> `dist\rl-app.exe = ~ RUNASADMIN`，导致即使 exe 已去掉 manifest 仍以管理员兼容层启动而
> 报 WinError 740。已用 PowerShell 删除该属性后恢复正常。**用户机器不受影响**（该键不会随包分发）。

源码改动清单（均已编译进 `dist/rl-app.exe` 17:12 版）：
`settings.rs`（新增）、`main.rs`（持久化映射 + `is_elevated` + `relaunch_admin` 命令 +
snapshot 增 `elevated`）、`Cargo.toml`（补 `serde_json`）、`frontend/index.html` +
`app.js` + `styles.css`（提权横幅）。`tools/check_manifest.py` 仅作 PE manifest 校验用，不影响构建。

### 11.7 紧急修复：非管理员/管理员点击限速控件整窗冻结（2026-11-10 18:30）

用户反馈「以管理员运行后，点击和限速有关的会卡死软件」。**与权限无关**，11.6 引入
`persist()` 时才带进来的回归。用 `tools/verify_no_freeze.mjs`（CDP 直接对每个 Tauri
命令计时）做对照实验，旧版实测：

```
[PASS] get_snapshot       OK 54ms
[FAIL] set_unit(MB/s)     TIMEOUT 3001ms   ← 自死锁
[FAIL] 之后所有命令        TIMEOUT           ← 锁永不释放，整窗冻结
settings.json  NOT FOUND
```

**根因 A：Mutex 自死锁（致命）**
`set_unit` 写成 `let mut g = state.lock().unwrap(); ... persist(&state);`，
这个**命名 guard 活到函数末尾**，而 `persist()` 内部又 `state.lock()` 一次。
`std::sync::Mutex` 不可重入 → 命令线程永久阻塞且锁永不释放；前台每 220ms 的
`get_snapshot`、后台线程、后续所有点击全部排队阻塞 → 整个窗口冻结。
其余命令用的是临时 guard（语句末即释放）才侥幸没死。
修复：所有命令统一「锁内改状态 + 锁内 `collect_settings()` 取快照 → **出锁后** `s.save()`」，
删掉 `persist()` 这个二次加锁辅助函数。

**根因 B：`set_rule` 参数绑定错误（隐藏更深）**
Rust 签名是单个 `edit: RuleEdit`，前端却一直传平铺参数，`call()` 的 `.catch()` 又把
`missing required key edit` 静默吞掉 → **挡位 chips / 方向开关 / 自定义输入从来没生效过**。
这才是上一轮「预设挡位没反应」的真凶之一。修复：前端三处改成 `act('set_rule', { edit })`，
并给 `call()` 加去重节流的前端错误 toast（`.err-toast`），杜绝「点了没反应又没提示」。

**根因 C：管理员下引擎真跑起来后的连锁风险（预防性加固）**
- `get_snapshot` 原在持锁时调 `fill_icons(24)`（`SHGetFileInfoW`/`GetDIBits`，慢 Win32）
  → 改为锁内取待办清单、**锁外提取**、回锁内写缓存；
- `IsUserAnAdmin()` 原在 `snapshot()` 里每 220ms 调一次 → 改为 `Inner.elevated` 启动时探一次；
- 后台线程原在「有包」分支完全不 sleep，持续流量下高频抢 shared 锁 → 统一让出 1ms；
- **所有加锁统一走 `lock_mutex()`（try_lock 轮询，200ms 超时）**，命令拿不到锁就跳过本轮，
  `get_snapshot` 返回 `None`（前端已有 `if (s) render(s)`，天然兼容，保留上一帧）。
  即便将来某处再次持锁不放，界面最多停顿 200ms，也不会整窗冻结。

**最终验证**（`node tools/verify_no_freeze.mjs`，修复版）：9 个命令全部 PASS（2–39ms），
`settings.json` 落盘 `unit=mb / master=true / other.down=p3+enabled`；
`tools/verify_ui.py` 主题切换、拖拽 `(119,96)`、截图均正常，无残留进程。

> 开发机又出现一次 `AppCompatFlags\Layers` 的 `RUNASADMIN` 残留导致 `spawn EACCES`
> （提权运行后 Windows 会重新写入），已再次删除。用户机器不受影响。



## 12. 架构级纠错（2026-11-10 19:20）：WinDivert 2.2 NETWORK 层不支持 processId

用户反馈「以管理员运行后，启用限速就报错」：
`引擎错误：打开 下行（c:\program files\google\chrome...）句柄失败` + 误报「请以管理员身份运行本程序」。

### 12.1 根因：整个按进程限速的 filter 方案从第一天起就是错的

WinDivert 2.2 官方文档（filter language 章节）明确写：

> The `processId` field ... **is not supported by the `WINDIVERT_LAYER_NETWORK*` layers**.
> Due to technical limitations, process ID information is not available at these layers.

`processId` 只在 **FLOW(2) / SOCKET(3) / REFLECT(4)** 层可用。而项目所有句柄都开在
`WINDIVERT_LAYER_NETWORK = 0`，却拼了 `processId == N`。用 dist 里真实 DLL 的
`WinDivertHelperCompileFilter` 离线编译（纯用户态，无需驱动/管理员）实测：

```
OK   base down/up                     (不含 processId)
FAIL ... (processId == 4242)          "Filter expression contains a bad token for layer" pos=54
FAIL ... processId != 100 ...         同上 pos=53
OK   processId == 4242                layer=2 (FLOW) 才通过
```

直接调 `WinDivertOpen` 对照（非管理员）：

| filter | GetLastError | 含义 |
|---|---|---|
| 新 base_filter（上/下行） | **5** | filter 合法，只是权限不足 ✔ |
| 旧 `processId == N` | **87** | filter 非法，**管理员也必然失败** ✘ |

所以之前非管理员时是「驱动加载失败」掩盖了 filter 错误；一提权就暴露。
33 个单测只校验了 filter **字符串怎么拼**，从没拿真实 DLL 编译过，因此一直没发现。

### 12.2 误报的来历
后端判断 `if e.contains("管理员")` 来决定要不要显示「请以管理员身份运行本程序」，
而 `Limiter::open` 的错误文案里硬编码了「请以管理员身份运行，并确认 WinDivert64.sys…」，
导致**任何**打开句柄失败都被归因为权限问题。已改为按真实 Win32 错误码分类
（2/5/87/577/654/1060/1275/1753 各给可操作建议），状态条也改为长错误换行完整显示。

### 12.3 另一处隐藏的致命 bug：WINDIVERT_ADDRESS 少 16 字节
`windivert.h` 里 `sizeof(WINDIVERT_ADDRESS) == 80`
（`INT64 Timestamp` 8 + `UINT32 位域块` 4 + `UINT32 Reserved2` 4 + 匿名 union 64），
项目一直写成 `Address([u8; 64])`。`WinDivertRecv` 写 80 字节 → 缓冲区溢出。
因为句柄从未打开成功、recv 从没被调用，这个 bug 被掩盖至今。已改为 80 字节，
并把大小写进 `tools/check_filter.py` 的静态断言。

### 12.4 新架构（用户选择「完整重构，保留按程序限速」）

```
每方向只开 1 个全局句柄（base_filter，不含 processId）
  → 解析 IPv4+TCP/UDP 五元组
  → GetExtendedTcpTable(TCP_TABLE_OWNER_PID_ALL) / GetExtendedUdpTable(UDP_TABLE_OWNER_PID)
    建立「规范化五元组 → owning PID」映射（每 700ms 刷新，不需要管理员）
  → PID → 程序 key（LimitConfig 的规则）→ 该程序/方向的令牌桶
  → 令牌足够则 WinDivertSend 重注入，不足则丢弃
```

- `crates/rl-engine-win/src/proc_map.rs`（新增）：`libloading` 动态加载 iphlpapi.dll，
  避开 GNU 工具链链接系统 .lib 的问题；TCP 表双向插入（正/反五元组都映射到同一 PID），
  UDP 表按 local endpoint 映射。注意 MIB 端口是网络序放 DWORD 低 16 位，需 `swap_bytes`。
- `crates/rl-engine-win/src/lib.rs` 重写：删掉 `wanted()/add_other()/MAX_EXCLUDES` 整套
  「多句柄 filter 不能重叠」逻辑（两个全局句柄 inbound/outbound 天然互斥，问题消失）。
  新增 `parse_tuple`（IPv4/TCP/UDP 头解析、非首片分片兜底）、`classify`、`pick_bucket`、
  `DirHandle`（每方向一个），以及打开句柄前的 `validate_filter`（用 DLL 离线编译校验）。
- `base_filter` 增加 `and not impostor`，避免重注入的包被自己再次捕获形成死循环。
- **限速稳定性**：`rate_signature` 故意不含 pids。进程扫描每 3 秒刷新 pids 会改变
  `LimitConfig`，若每次都重建桶就会把令牌重置为满、曲线清零；现在只在速率真变化时
  复用旧桶并 `set_rate`，pids 变化只更新 PID→key 映射。
- `rl-core` 里 `build_filter` / `build_program_filter` 及 `FILTER_MAX_LEN` / `MAX_EXCLUDES`
  全部删除（会生成非法 filter，留着是隐患），`Direction::filter()` 别名一并删除。

### 12.5 验证

- `cargo test --workspace`：**35 个单测全绿**（rl-core 22 + engine 13），
  含 `real_connection_tables_load` 真实 FFI 冒烟（GetExtendedTcpTable 不需要管理员）。
  测试总数下降是因为删掉了 7 个「校验非法 filter 字符串怎么拼」的旧测试，
  换成 `parse_tuple` / `classify` / `pick_bucket` / 连接表解析等新架构测试。
- `python tools/check_filter.py`：用真实 DLL 离线编译 rl-core 源码里抽出的两个
  `base_filter`，静态断言引擎代码无 `processId ==/!=`、`WINDIVERT_ADDRESS == 80`，
  并用反例确认 DLL 确实会拒绝 processId（保证校验有判别力）。全部 PASS。
- `node tools/verify_no_freeze.mjs`：9 个命令全 PASS（2–73ms）、持久化落盘正确。
- `node tools/diag_engine.mjs`（非管理员）：错误精确为
  `打开下行全局句柄失败（Win32 错误 5）：权限不足…`，提示「请以管理员身份运行本程序」——
  证明新 filter 已通过编译阶段、只被权限拦住，且文案不再误报。
- `python tools/verify_ui.py`：明暗主题、拖拽 `(120,60)`、截图正常，无残留进程。

> 开发机当前会话非管理员（`IsInRole(Administrator) = False`），**驱动加载后的真实限速
> 速率无法在本机实测**；已验证到「filter 合法 + 只差权限」这一层。用户提权后即可生效。

### 12.6 仍需在真机确认
- 提权后下行/上行速率是否稳定在设定值 ±10%（计划用下载大文件 / iperf3 观察曲线）
- 进程归属映射对短连接、UDP、TIME_WAIT 的覆盖准确度
- 安全软件是否会拦截 WinDivert64.sys（错误 1275 已给出白名单提示）

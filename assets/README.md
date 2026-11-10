# assets 目录

WinDivert 运行时（v2.2.2，MIT 许可，官方签名驱动）已下载好，直接随程序分发：

| 文件 | 说明 |
|---|---|
| `WinDivert.dll` | 用户态库，程序运行时动态加载（不是编译期链接） |
| `WinDivert64.sys` | 内核驱动，首次运行时由 WinDivert 自动安装 |
| `WinDivert.lib` | MSVC 链接用，本项目用不到（动态加载），保留备用 |

## 发布时的目录结构

```
rl-app.exe
WinDivert.dll
WinDivert64.sys
```

**必须把 dll 和 sys 与 exe 放在同一目录**，否则程序会提示「加载 WinDivert.dll 失败」。

## 权限

加载驱动需要管理员权限。当前 exe 还没嵌 manifest，运行时请**右键 → 以管理员身份运行**。
后续要加自动提权：在 `rl-app` 里用 `embed-resource`/`winres` 嵌入 `requireAdministrator` 的 manifest，
或在启动时检测权限不足后用 `ShellExecuteW("runas", ...)` 重启自身。

## 验证是否生效

限下行 1 MB/s 后下载大文件，或：

```powershell
# 简单测速：连续下载 20 秒，观察平均速率
curl -o $null https://speed.cloudflare.com/__down?bytes=100000000
```

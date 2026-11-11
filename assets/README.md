# assets 目录

本目录的**图标资源**随仓库入库；**运行时二进制**(`WinDivert.dll` / `WinDivert64.sys` / `WebView2Loader.dll`)不入库(见根目录 `.gitignore`),由下面两种方式提供：

- **CI 构建**：`tools/ci/build-win.ps1` 发现缺失时自动从 GitHub Release 附件 zip 下载解出
- **本地开发**：手动从 [WinDivert 官方](https://www.webinterface.nl/software/windivert)(v2.2.2-A,MIT 许可,微软签名驱动)与 WebView2 SDK 取文件放到本目录

| 文件 | 说明 |
|---|---|
| `WinDivert.dll` | 用户态库,程序运行时动态加载 |
| `WinDivert64.sys` | 内核驱动,首次运行时由 WinDivert 自动安装 |
| `WinDivert.lib` | MSVC 链接用 import library,**编译期需要**(windivert-sys 通过 `WINDIVERT_PATH` 找到它);发布 zip 里不含它 |
| `WebView2Loader.dll` | Tauri/WebView2 加载器 |

> 编译期/CI 取文件来源:GitHub Release 附件(`WinDivert.lib` 为独立附件,其余在 zip 内);
> 本地开发把四个文件放进本目录一次即可。

## 发布时的目录结构

```
FlowRule.exe
WinDivert.dll
WinDivert64.sys
WebView2Loader.dll
```

**必须把这三个 dll/sys 与 exe 放在同一目录**,否则程序会提示加载 WinDivert.dll 失败。

## 权限

加载驱动需要管理员权限。exe 已内嵌 `requireAdministrator` manifest,**双击即自动弹出 UAC 提权**,无需右键。

## 验证是否生效

对某程序限下行 1 MB/s 后下载大文件,或：

```powershell
# 简单测速：连续下载约 100 MB,观察平均速率
curl.exe -o $null https://speed.cloudflare.com/__down?bytes=100000000
```

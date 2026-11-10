# 在 CNB 自托管 Windows 构建节点上编译流律（liulv-ratelimiter）
#
# 前置条件（一次性，在接入的 Windows 机器上准备）：
#   1. Rust 工具链（stable-x86_64-pc-windows-msvc）；若未安装，本脚本会自动用 rustup 安装。
#   2. Visual Studio Build Tools（MSVC 链接器 + Windows SDK），用于链接 exe。
#   3. git + tar（Windows 10/11 自带 bsdtar）。
#   4. 在 CNB「tsbyj.666 / 组织设置 / 构建节点」接入该机器，标签至少包含 `windows`。
#
# 产物：rl-app.exe + WinDivert.dll/WinDivert64.sys/WebView2Loader.dll 打成 zip，
#      由 tools/ci/upload-artifact.ps1 上传到 CNB。
#
# 注意：CNB 自托管 Windows 节点的 stage script 默认按 PowerShell 执行，
#       因此这里所有命令都写成 PowerShell 语法。

param(
    [string]$OutDir = "dist"
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$workspace = (Resolve-Path $env:CNB_BUILD_WORKSPACE).Path
Set-Location $workspace

function Write-Step($msg) { Write-Host "==> $msg" }

# ---------------------------------------------------------------- 0. 环境自检
Write-Step "工作区: $workspace"

# 0.1 对齐到触发本次构建的提交
#     pull_request.merged 事件的 CNB_COMMIT 是合并后的 SHA，工作区可能还停在别处。
$target = $env:CNB_COMMIT
if (-not $target) { $target = 'HEAD' }
try {
    git rev-parse --verify "$target^{commit}" *> $null
    if ($LASTEXITCODE -ne 0) { git fetch --no-tags origin $target *> $null }
    if ((git cat-file -t $target 2>$null) -eq 'commit') {
        git checkout --detach --force $target
        Write-Step "已 checkout 到 $target"
    } else {
        Write-Step "本地找不到 $target，沿用工作区现状"
        $target = 'HEAD'
    }
} catch {
    Write-Step "对齐提交失败（$($_.Exception.Message)），沿用工作区现状"
    $target = 'HEAD'
}

# 0.2 源码搬到纯 ASCII 路径下再编译
#     本仓库路径含中文，MSVC/GNU 工具链在部分环节（资源编译、链接）会因
#     非 ASCII 路径失败；仓库内 .cargo/config.toml 也因此把 target-dir 写死。
#     这里用 git archive 导出一份到 %TEMP% 下的纯 ASCII 目录，彻底规避。
$asciiSrc = Join-Path $env:TEMP 'liulv-src'
if (Test-Path $asciiSrc) { Remove-Item -LiteralPath $asciiSrc -Recurse -Force }
New-Item -ItemType Directory -Force -Path $asciiSrc | Out-Null
$tarPath = Join-Path $env:TEMP 'liulv-src.tar'
if (Test-Path $tarPath) { Remove-Item -LiteralPath $tarPath -Force }
git archive --format=tar -o $tarPath $target
if ($LASTEXITCODE -ne 0) { throw "git archive 失败（exit $LASTEXITCODE）" }
tar -xf $tarPath -C $asciiSrc
Remove-Item -LiteralPath $tarPath -Force
Write-Step "源码已导出到纯 ASCII 路径：$asciiSrc"

# 0.3 Rust 工具链
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Step "未检测到 cargo，开始安装 rustup (stable-x86_64-pc-windows-msvc)"
    $rustup = Join-Path $env:TEMP 'rustup-init.exe'
    Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $rustup -UseBasicParsing
    & $rustup -y --default-toolchain stable-x86_64-pc-windows-msvc --profile minimal
    $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
}
Write-Step ("cargo: " + (& cargo --version))

# 0.4 MSVC 链接器自检：没有 link.exe 就无法产出 exe
$hasMsvc = $false
foreach ($p in @(
    "${env:ProgramFiles}\Microsoft Visual Studio",
    "${env:ProgramFiles(x86)}\Microsoft Visual Studio"
)) {
    if (Test-Path $p) {
        $hit = Get-ChildItem -Path $p -Filter link.exe -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match 'Hostx64\\x64' } |
            Select-Object -First 1
        if ($hit) { $hasMsvc = $true; $msvcLink = $hit.FullName; break }
    }
}
if (-not $hasMsvc) {
    throw "未找到 MSVC 链接器 link.exe。请在构建机上安装 Visual Studio Build Tools（工作负载：使用 C++ 的桌面开发）。"
}
Write-Step "MSVC link.exe: $msvcLink"

# target-dir 用环境变量覆盖到纯 ASCII 路径（仓库内 config.toml 里写死的是本机路径）
$targetDir = Join-Path $env:TEMP 'liulv-target'
$env:CARGO_TARGET_DIR = $targetDir
Write-Step "CARGO_TARGET_DIR: $targetDir"

# ---------------------------------------------------------------- 1. 构建
Push-Location $asciiSrc
try {
    Write-Step "cargo build --release --workspace"
    & cargo build --release --workspace
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }

    $exe = Join-Path $targetDir 'release\rl-app.exe'
    if (-not (Test-Path $exe)) { throw "未找到构建产物：$exe" }
    Write-Step ("rl-app.exe 大小: " + (Get-Item $exe).Length + " 字节")
} finally {
    Pop-Location
}

# ---------------------------------------------------------------- 2. 收集运行时文件
$outFull = Join-Path $workspace $OutDir
New-Item -ItemType Directory -Force -Path $outFull | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $outFull 'rl-app.exe') -Force

foreach ($f in @('WinDivert.dll', 'WinDivert64.sys', 'WebView2Loader.dll')) {
    $src = Join-Path $asciiSrc ("assets\" + $f)
    if (-not (Test-Path $src)) { throw "缺少运行时文件：$src" }
    Copy-Item -LiteralPath $src -Destination (Join-Path $outFull $f) -Force
}
Write-Step "运行时文件已收集到 $outFull"

# ---------------------------------------------------------------- 3. 打包 zip
$ver = (Select-String -Path (Join-Path $asciiSrc 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$zipName = "liulv-v$ver-win-x64.zip"
$zipPath = Join-Path $workspace $zipName
if (Test-Path $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -Path (Join-Path $outFull '*') -DestinationPath $zipPath -Force
Write-Step ("已打包：$zipName（" + (Get-Item $zipPath).Length + " 字节）")

# 供后续 stage 使用的元数据
Set-Content -LiteralPath (Join-Path $outFull 'artifact.json') -Encoding UTF8 -Value (@{
    name    = $zipName
    path    = $zipPath
    size    = (Get-Item $zipPath).Length
    version = $ver
} | ConvertTo-Json)

Write-Step "构建完成：$zipName"

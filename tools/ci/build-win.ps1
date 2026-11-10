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

# 编码：本文件以 UTF-8 with BOM 保存，Windows PowerShell 5.1 才能正确识别中文；
# 再把控制台输出也切到 UTF-8，避免日志里的中文在控制台变成问号。
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding = [System.Text.Encoding]::UTF8
} catch { }

$workspace = (Resolve-Path $env:CNB_BUILD_WORKSPACE).Path
Set-Location $workspace

function Write-Step($msg) { Write-Host "==> $msg" }

# ---------------------------------------------------------------- 0. 环境自检
Write-Step "工作区: $workspace"

# 0.1 确认触发本次构建的提交在本地可见
#     pull_request.merged 事件的 CNB_COMMIT 是合并后的 SHA。
#     不做 checkout（避免把工作区切成 detached HEAD 影响后续 stage），
#     真正编译用的是下面 git archive 导出的干净副本。
$target = $env:CNB_COMMIT
if (-not $target) { $target = 'HEAD' }
try {
    git rev-parse --verify "$target^{commit}" *> $null
    if ($LASTEXITCODE -ne 0) { git fetch --no-tags origin $target *> $null }
    if ((git cat-file -t $target 2>$null) -eq 'commit') {
        Write-Step "目标提交：$target"
    } else {
        Write-Step "本地找不到 $target，改用 HEAD"
        $target = 'HEAD'
    }
} catch {
    Write-Step "定位提交失败（$($_.Exception.Message)），改用 HEAD"
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
#     CNB 自托管 runner 常以 LocalSystem 账户运行，%USERPROFILE% 指向
#     C:\Windows\System32\config\systemprofile，而 Rust 往往装在某个真人用户下
#     （本机为 C:\Users\MOSS）。因此这里不只看当前账户，而是扫描所有用户目录；
#     优先直接使用 rustup 工具链里的真实 cargo.exe（不依赖 shim，也就不依赖 RUSTUP_HOME）。
$cargoExe = $null
$cargoHome = $null
$rustupHome = $null

$userRoots = @(Get-ChildItem 'C:\Users' -Directory -ErrorAction SilentlyContinue |
                ForEach-Object { $_.FullName })

$cargoCandidates = New-Object System.Collections.ArrayList
# 1) rustup 工具链里的**真实** cargo.exe —— 首选。
#    它不经过 rustup shim，不需要 default toolchain 配置，最稳。
foreach ($u in $userRoots) {
    $rh = Join-Path $u '.rustup\toolchains'
    if (-not (Test-Path -LiteralPath $rh -ErrorAction SilentlyContinue)) { continue }
    foreach ($tc in (Get-ChildItem -LiteralPath $rh -Directory -ErrorAction SilentlyContinue)) {
        [void]$cargoCandidates.Add((Join-Path $tc.FullName 'bin\cargo.exe'))
    }
}
# 2) 环境变量显式指定
if ($env:CNB_CARGO_HOME) {
    [void]$cargoCandidates.Add((Join-Path $env:CNB_CARGO_HOME 'bin\cargo.exe'))
}
# 3) 各用户目录下的 rustup shim（可能因没设 default toolchain 而不可用，靠下面的探测排除）
foreach ($u in @($env:USERPROFILE) + $userRoots) {
    [void]$cargoCandidates.Add((Join-Path $u '.cargo\bin\cargo.exe'))
}

# 逐个候选实跑 `cargo --version`，只接受真能用的那个。
# 注意：不能只判断文件存在 —— rustup shim 在没有 default toolchain 时是「存在但不可用」的，
# 之前就是踩了这个坑：systemprofile 下有一次装失败的半成品 shim，被选中后直接报错。
foreach ($c in $cargoCandidates) {
    if (-not (Test-Path -LiteralPath $c -ErrorAction SilentlyContinue)) { continue }
    $ver = ''
    try {
        $ver = (& $c --version 2>&1 | Out-String).Trim()
    } catch { $ver = '' }
    if ($ver -match '^cargo\s+\d') {
        $cargoExe = $c
        Write-Step "cargo 候选可用：$c（$ver）"
        break
    }
}

if (-not $cargoExe) {
    Write-Step "未找到可用的 cargo，尝试用 rustup 安装 (stable-x86_64-pc-windows-msvc)"
    $rustup = Join-Path $env:TEMP 'rustup-init.exe'
    Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $rustup -UseBasicParsing
    & $rustup -y --default-toolchain stable-x86_64-pc-windows-msvc --profile minimal
    if ($LASTEXITCODE -ne 0) { throw "rustup 安装失败（exit $LASTEXITCODE）" }
    $cargoExe = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (-not (Test-Path -LiteralPath $cargoExe -ErrorAction SilentlyContinue)) {
        throw "rustup 安装后仍找不到 cargo：$cargoExe"
    }
}

# 推断 CARGO_HOME / RUSTUP_HOME
#   cargo.exe 位于 <home>\.cargo\bin\          → home = <home>
#   cargo.exe 位于 <home>\.rustup\toolchains\<tc>\bin\ → home = <home>
$toolchainBin = Split-Path -Parent $cargoExe
$userHome = $null
$tcRoot = Split-Path -Parent (Split-Path -Parent $toolchainBin)
if ((Split-Path -Leaf $tcRoot) -eq 'toolchains') {
    # <home>\.rustup\toolchains\<tc>\bin\cargo.exe
    $userHome = Split-Path -Parent (Split-Path -Parent $tcRoot)
    $rustupHome = Join-Path $userHome '.rustup'
} else {
    # <home>\.cargo\bin\cargo.exe（rustup shim）
    $cargoHome = Split-Path -Parent $toolchainBin
    $userHome = Split-Path -Parent $cargoHome
}
if (-not $cargoHome -and $userHome) { $cargoHome = Join-Path $userHome '.cargo' }
if (-not $rustupHome -and $userHome) { $rustupHome = Join-Path $userHome '.rustup' }
if ($cargoHome -and (Test-Path -LiteralPath $cargoHome -ErrorAction SilentlyContinue)) { $env:CARGO_HOME = $cargoHome }
if ($rustupHome -and (Test-Path -LiteralPath $rustupHome -ErrorAction SilentlyContinue)) { $env:RUSTUP_HOME = $rustupHome }

# 把工具链 bin 加进 PATH，rustc 与 link.exe 配套环境都在同级
if (Test-Path (Join-Path $toolchainBin 'rustc.exe')) {
    $env:Path = "$toolchainBin;$env:Path"
}
Write-Step "cargo 可执行文件：$cargoExe"
Write-Step "CARGO_HOME：$env:CARGO_HOME"
Write-Step "RUSTUP_HOME：$env:RUSTUP_HOME"
Write-Step ("cargo: " + (& $cargoExe --version))

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
    & $cargoExe build --release --workspace
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }

    $exe = Join-Path $targetDir 'release\rl-app.exe'
    if (-not (Test-Path $exe)) { throw "未找到构建产物：$exe" }
    Write-Step ("rl-app.exe 大小: " + (Get-Item $exe).Length + " 字节")
} finally {
    Pop-Location
}

# ---------------------------------------------------------------- 2. 收集运行时文件
# 在 %TEMP% 下的独立暂存目录组装，避免工作区里的 dist\rl-app.exe 正被运行中的
# 程序占用（Copy-Item 会因文件锁直接失败），也避免污染仓库工作区。
$stage = Join-Path $env:TEMP 'liulv-stage'
if (Test-Path $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'rl-app.exe') -Force

foreach ($f in @('WinDivert.dll', 'WinDivert64.sys', 'WebView2Loader.dll')) {
    $src = Join-Path $asciiSrc ("assets\" + $f)
    if (-not (Test-Path $src)) { throw "缺少运行时文件：$src" }
    Copy-Item -LiteralPath $src -Destination (Join-Path $stage $f) -Force
}
Write-Step "运行时文件已收集到 $stage"

# 同时把产物放一份到工作区 dist/ 供人工取用；文件可能被占用，放不下不致命。
$outFull = Join-Path $workspace $OutDir
New-Item -ItemType Directory -Force -Path $outFull | Out-Null
try {
    Copy-Item -Path (Join-Path $stage '*') -Destination $outFull -Force -ErrorAction Stop
    Write-Step "产物已复制到 $outFull"
} catch {
    Write-Step "工作区 dist/ 被占用，跳过复制（不影响打包与上传）：$($_.Exception.Message)"
}

# ---------------------------------------------------------------- 3. 打包 zip
$ver = (Select-String -Path (Join-Path $asciiSrc 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$zipName = "liulv-v$ver-win-x64.zip"
$zipPath = Join-Path $workspace $zipName
if (Test-Path $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipPath -Force
Write-Step ("已打包：$zipName（" + (Get-Item $zipPath).Length + " 字节）")

# 供后续 stage 使用的元数据
# 用 UTF-8 无 BOM 写盘：后续 upload-artifact.ps1 按 UTF-8 显式读取，
# 避免 Windows PowerShell 5.1 的 Set-Content -Encoding UTF8 写入 BOM 后解析 JSON 出错。
$meta = @{
    name    = $zipName
    path    = $zipPath
    size    = (Get-Item $zipPath).Length
    version = $ver
} | ConvertTo-Json
[System.IO.File]::WriteAllText(
    (Join-Path $workspace 'dist\artifact.json'),
    $meta,
    (New-Object System.Text.UTF8Encoding($false))
)

Write-Step "构建完成：$zipName"

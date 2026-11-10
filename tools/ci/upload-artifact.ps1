# 把构建好的 zip 上传到 CNB：
#   -Mode commit  → 作为本次提交的「附件」（commit assets），在提交页/仓库页可下载
#   -Mode release → 创建（或复用）对应 Tag 的 Release，并把 zip 作为 Release 附件
#
# 之所以不用 cnbcool/attachments 插件：那是 Docker 镜像插件，
# 而自托管 Windows 构建节点默认不开启 Docker 能力，这里直接走 OpenAPI。

param(
    [ValidateSet('commit', 'release')]
    [string]$Mode = 'commit'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location $root

$meta = Get-Content -LiteralPath (Join-Path $root 'dist\artifact.json') -Raw -Encoding UTF8 | ConvertFrom-Json
$file = $meta.path
if (-not (Test-Path -LiteralPath $file)) { throw "产物不存在：$file" }
$size = (Get-Item -LiteralPath $file).Length
$name = $meta.name

$api = if ($env:CNB_API_ENDPOINT) { $env:CNB_API_ENDPOINT.TrimEnd('/') } else { 'https://api.cnb.cool' }
$repo = $env:CNB_REPO_SLUG
$token = $env:CNB_TOKEN
if (-not $repo) { throw '缺少环境变量 CNB_REPO_SLUG' }
if (-not $token) { throw '缺少环境变量 CNB_TOKEN' }

$hdr = @{ Authorization = "Bearer $token"; Accept = 'application/json' }
Write-Host "==> 仓库 $repo | 模式 $Mode | 产物 $name ($size 字节)"

function Post-Json($url, $obj) {
    $json = $obj | ConvertTo-Json -Depth 6 -Compress
    Invoke-RestMethod -Uri $url -Method Post -Headers $hdr `
        -ContentType 'application/json; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($json))
}

# CNB 部分接口把结果包在 data 里，部分直接返回，取值时两种都兼容
function Get-Field($obj, [string]$name) {
    if ($null -eq $obj) { return $null }
    if ($obj.PSObject.Properties.Name -contains $name) { return $obj.$name }
    if ($null -ne $obj.data -and ($obj.data.PSObject.Properties.Name -contains $name)) { return $obj.data.$name }
    return $null
}

# 从 verify_url 里取出 upload_token 与 asset_path
function Split-VerifyUrl($verifyUrl) {
    $u = [Uri]$verifyUrl
    $seg = $u.AbsolutePath.Trim('/').Split('/')
    return @{
        Token     = $seg[$seg.Length - 2]
        AssetPath = [Uri]::UnescapeDataString($seg[$seg.Length - 1])
    }
}

function Upload-Binary($uploadUrl, $verifyUrl, $localFile, $confirmUrl) {
    $code = & curl.exe -sS -o NUL -w '%{http_code}' -X PUT `
        -H "Authorization: Bearer $token" `
        -H 'Content-Type: application/octet-stream' `
        --data-binary "@$localFile" $uploadUrl
    if ($code -ne '200') { throw "上传二进制失败，HTTP $code" }
    Write-Host "    二进制已上传（HTTP 200）"

    $parts = Split-VerifyUrl $verifyUrl
    $url = $confirmUrl -replace '\{token\}', $parts.Token -replace '\{path\}', [Uri]::EscapeDataString($parts.AssetPath)
    Invoke-RestMethod -Uri $url -Method Post -Headers $hdr | Out-Null
    Write-Host "    附件已确认：$($parts.AssetPath)"
}

switch ($Mode) {

    'commit' {
        $sha = $env:CNB_COMMIT
        if (-not $sha) { throw '缺少环境变量 CNB_COMMIT' }
        $r = Post-Json "$api/$repo/-/git/commit-assets/$sha/asset-upload-url" `
            @{ asset_name = $name; size = $size; ttl = 0 }
        $uploadUrl = Get-Field $r 'upload_url'
        $verifyUrl = Get-Field $r 'verify_url'
        Upload-Binary $uploadUrl $verifyUrl $file `
            "$api/$repo/-/git/commit-assets/$sha/asset-upload-confirmation/{token}/{path}?ttl=0"
        Write-Host "==> 提交附件上传完成：$name"
        Write-Host "    下载：$api/$repo/-/commit-assets/download/$sha/$name"
    }

    'release' {
        $tag = $env:CNB_BRANCH
        if (-not $tag) { throw '缺少环境变量 CNB_BRANCH（Tag 名）' }

        # 1) 复用已有 Release，没有则创建
        $releaseId = $null
        try {
            $existing = Invoke-RestMethod -Uri "$api/$repo/-/releases/tags/$tag" -Headers $hdr
            $releaseId = Get-Field $existing 'id'
            Write-Host "==> 复用已有 Release $tag（id=$releaseId）"
        } catch {
            Write-Host "==> Release $tag 不存在，创建中"
            $body = @{
                tag_name         = $tag
                name             = $tag
                target_commitish = $env:CNB_COMMIT
                body             = "Windows 单程序网络限速工具（Rust + Tauri + WinDivert）。`n`n- 限速只作用于目标程序，不再整机被限`n- 自定义速率（如 20MB）真实按 20MB/s 生效`n- 引擎改用 WinDivertRecvEx 重叠 I/O + 合法 QUEUE_TIME(100ms)`n- 支持 IPv6 / QUIC 流量归属"
                draft            = $false
                prerelease       = $false
            }
            $created = Post-Json "$api/$repo/-/releases" $body
            $releaseId = Get-Field $created 'id'
            Write-Host "==> Release $tag 创建成功（id=$releaseId）"
        }
        if (-not $releaseId) { throw "未能确定 Release id（tag=$tag）" }

        # 2) 上传 zip 作为 Release 附件
        $r = Post-Json "$api/$repo/-/releases/$releaseId/asset-upload-url" `
            @{ asset_name = $name; size = $size; overwrite = $true; ttl = 0 }
        $uploadUrl = Get-Field $r 'upload_url'
        $verifyUrl = Get-Field $r 'verify_url'
        Upload-Binary $uploadUrl $verifyUrl $file `
            "$api/$repo/-/releases/$releaseId/asset-upload-confirmation/{token}/{path}?ttl=0"
        Write-Host "==> Release 附件上传完成：$name"
        Write-Host "    下载：$api/$repo/-/releases/download/$tag/$name"
    }
}

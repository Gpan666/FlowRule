# After the CNB tag_push build succeeds, sync the release zip to GitHub:
#   - find (or create) the GitHub release for the same tag
#   - upload the zip as a release asset (replacing a same-named old asset)
#
# Requires env:
#   GITHUB_TOKEN  personal access token with repo write scope on the target
#                 repo (configure it as a pipeline env var in CNB settings;
#                 never commit it)
#   CNB_BRANCH    tag name carried by the tag_push event
# Optional env:
#   GITHUB_REPO   target "owner/repo", defaults to Gpan666/FlowRule
#
# Comments are English-only on purpose: the self-hosted node may run
# PowerShell 5.1, which mis-parses non-BOM UTF-8 Chinese text.

param(
    [string]$GithubRepo = 'Gpan666/FlowRule'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
} catch { }

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location $root

$token = $env:GITHUB_TOKEN
if (-not $token) {
    throw 'GITHUB_TOKEN is not set. Add it in CNB repo settings (pipeline env vars) as a secret.'
}

$metaPath = Join-Path $root 'dist\artifact.json'
$metaJson = [System.IO.File]::ReadAllText($metaPath, (New-Object System.Text.UTF8Encoding($false)))
$meta = $metaJson | ConvertFrom-Json
$file = $meta.path
$name = $meta.name
if (-not (Test-Path -LiteralPath $file)) { throw "artifact not found: $file" }

$tag = $env:CNB_BRANCH
if (-not $tag) { throw 'CNB_BRANCH (tag name) is missing' }

# If the repo slug was overridden, honor it.
if ($env:GITHUB_REPO) { $GithubRepo = $env:GITHUB_REPO }

$H = @{ Authorization = "token $token"; 'User-Agent' = 'cnb-flowrule-ci'; Accept = 'application/json' }
$api = 'https://api.github.com'

# 1) find or create the release for this tag
$rel = $null
try {
    $rel = Invoke-RestMethod -Uri "$api/repos/$GithubRepo/releases/tags/$tag" -Headers $H
    Write-Host "GitHub release $tag already exists"
} catch {
    $rel = $null
}
if (-not $rel) {
    $bodyPath = Join-Path $root 'tools\ci\release-body.txt'
    $body = [System.IO.File]::ReadAllText($bodyPath, (New-Object System.Text.UTF8Encoding($false)))
    $json = @{
        tag_name   = $tag
        name       = "FlowRule $tag"
        body       = $body
        draft      = $false
        prerelease = $false
    } | ConvertTo-Json
    $rel = Invoke-RestMethod -Method Post -Uri "$api/repos/$GithubRepo/releases" `
        -Headers $H -ContentType 'application/json; charset=utf-8' `
        -Body ([Text.Encoding]::UTF8.GetBytes($json))
    Write-Host "created GitHub release $tag"
}

# 2) replace a same-named asset if present, then upload the zip
foreach ($a in @($rel.assets)) {
    if ($a -and $a.name -eq $name) {
        Invoke-RestMethod -Method Delete -Uri "$api/repos/$GithubRepo/releases/assets/$($a.id)" -Headers $H | Out-Null
        Write-Host "removed old asset $name"
    }
}
$upUrl = "https://uploads.github.com/repos/$GithubRepo/releases/$($rel.id)/assets?name=$name"
Invoke-RestMethod -Method Post -Uri $upUrl `
    -Headers @{ Authorization = "token $token"; 'User-Agent' = 'cnb-flowrule-ci'; 'Content-Type' = 'application/zip' } `
    -InFile $file | Out-Null
Write-Host "uploaded $name to GitHub release $tag"

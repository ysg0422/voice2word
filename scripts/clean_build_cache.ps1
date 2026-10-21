# 清理 Rust 构建缓存（释放磁盘，不碰源码与 release 产物）
#
# 背景：`target\debug\incremental` 是**无上限增长**的增量编译缓存。改代码越频繁、
#      切换 profile 越多，它叠得越大（本项目一度到 27 GB，把 A 盘吃到只剩 5 GB）。
#      Rust 不会自动回收它，只能手动删。
#
# 用法：
#   pwsh -File scripts/clean_build_cache.ps1              # 只删 incremental（最安全）
#   pwsh -File scripts/clean_build_cache.ps1 -Deep        # 连 debug/deps、build 一起删
#   pwsh -File scripts/clean_build_cache.ps1 -Release     # 连 target/release 一起删（打包前须重新 build）
#   pwsh -File scripts/clean_build_cache.ps1 -ThresholdGB 8   # 低于阈值不动手（默认 8）
#
# 不删：源码、models/、testVideo/、dist/、.git/。

param(
    [switch]$Deep,
    [switch]$Release,
    [double]$ThresholdGB = 8
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$target = Join-Path $root "target"
if (-not (Test-Path $target)) { Write-Host "没有 target 目录：$target"; exit 0 }

function Get-SizeGB($path) {
    if (-not (Test-Path $path)) { return 0 }
    $sum = (Get-ChildItem -LiteralPath $path -Recurse -File -Force -ErrorAction SilentlyContinue |
            Measure-Object Length -Sum).Sum
    if (-not $sum) { return 0 }
    return [math]::Round($sum / 1GB, 2)
}

$incremental = Join-Path $target "debug\incremental"
$size = Get-SizeGB $incremental
Write-Host ("target\debug\incremental = {0} GB" -f $size)

$freed = 0
if ($size -ge $ThresholdGB) {
    Write-Host "超过阈值 $ThresholdGB GB，删除中…"
    Remove-Item -LiteralPath $incremental -Recurse -Force -ErrorAction SilentlyContinue
    $freed += $size
} else {
    Write-Host "未超过阈值，跳过。"
}

if ($Deep) {
    foreach ($dir in @("debug\deps", "debug\build", "debug\examples")) {
        $p = Join-Path $target $dir
        $s = Get-SizeGB $p
        if ($s -gt 0) {
            Write-Host ("删除 {0}（{1} GB）…" -f $dir, $s)
            Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue
            $freed += $s
        }
    }
}

if ($Release) {
    $p = Join-Path $target "release"
    $s = Get-SizeGB $p
    if ($s -gt 0) {
        Write-Host ("删除 release（{0} GB）…之后打包需重新 cargo build --release" -f $s)
        Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue
        $freed += $s
    }
}

$drive = (Get-Item $root).PSDrive
Write-Host ("释放约 {0} GB；{1}: 剩余 {2} GB" -f $freed, $drive.Name, [math]::Round($drive.Free / 1GB, 1))

<#
.SYNOPSIS
  Voice2Word release 构建（自动解决 gpui 的 fxc.exe 依赖）。

.DESCRIPTION
  为什么需要这个脚本：gpui 0.2.2 的 build.rs 在 **release** 构建时要调 fxc.exe 编译
  着色器（debug 走运行时 D3DCompileFromFile，不受影响）。它的查找顺序是：

      1. 环境变量 GPUI_FXC_PATH
      2. `where.exe fxc.exe`
      3. C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\fxc.exe
      → 三项全灭则 panic!("Failed to find fxc.exe")

  仓库里**刻意不写死任何机器路径**（.cargo/config.toml 只留注释示例），因此：
    - CI（windows-latest）自带 Windows SDK 在默认位置 → 直接 `cargo build --release` 即可；
    - 本机若把 Windows SDK 装在非常规位置（例如 D 盘）→ 需要设 GPUI_FXC_PATH，
      这正是本脚本负责的事：自动探测并**只在本次进程内**设置，不落盘、不进仓库。

.PARAMETER ExtraArgs
  透传给 cargo 的额外参数，例如 -ExtraArgs '--features','xxx'。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build_release.ps1
#>
[CmdletBinding()]
param(
    [string[]]$ExtraArgs = @()
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path

function Find-Fxc {
    # 1) 已显式指定（CI / 用户手设）优先
    if ($env:GPUI_FXC_PATH -and (Test-Path -LiteralPath $env:GPUI_FXC_PATH)) {
        return $env:GPUI_FXC_PATH
    }
    # 2) PATH 里能直接找到
    $where = Get-Command fxc.exe -ErrorAction SilentlyContinue
    if ($where) { return $where.Source }

    # 3) 扫常见 Windows SDK 安装根（含 C 盘默认与 D 盘等非常规位置）。
    #    取「版本号最大的那一档」，与 gpui 硬编码 10.0.26100.0 的偏好一致。
    $roots = @(
        'C:\Program Files (x86)\Windows Kits\10\bin',
        'C:\Program Files\Windows Kits\10\bin'
    )
    # 从磁盘根枚举其它盘符上的 SDK（用户可能装到 D:/E:）
    foreach ($drive in (Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Root -match '^[A-Za-z]:\\$' })) {
        $roots += (Join-Path $drive.Root 'Windows Kits\10\bin')
    }

    $candidates = @()
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
            ForEach-Object {
                $exe = Join-Path $_.FullName 'x64\fxc.exe'
                if (Test-Path -LiteralPath $exe) { $candidates += $exe }
            }
    }
    if ($candidates.Count -gt 0) {
        # 版本号降序：10.0.26100.0 > 10.0.22621.0 …
        $best = $candidates | Sort-Object {
            $ver = [regex]::Match($_, '(\d+\.\d+\.\d+\.\d+)').Groups[1].Value
            [version]$ver
        } -Descending | Select-Object -First 1
        return $best
    }
    return $null
}

$fxc = Find-Fxc
if ($fxc) {
    $env:GPUI_FXC_PATH = $fxc
    Write-Host "[fxc] 使用 $fxc" -ForegroundColor DarkGray
} else {
    Write-Warning "未找到 fxc.exe：release 构建会在 gpui 的 build script 处 panic。"
    Write-Warning "请安装「Windows SDK」（含 Windows SDK for Desktop C++ x86/x64），"
    Write-Warning "或手动设置环境变量 GPUI_FXC_PATH 指向 fxc.exe。"
}

Write-Host "[build] cargo build --release" -ForegroundColor Cyan
Push-Location $repoRoot
try {
    if ($ExtraArgs.Count -gt 0) {
        cargo build --release @ExtraArgs
    } else {
        cargo build --release
    }
    $code = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($code -ne 0) { exit $code }
Write-Host "[build] 完成 → target\release\voice2word.exe" -ForegroundColor Green
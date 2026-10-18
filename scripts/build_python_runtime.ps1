# 构建内置便携 Python 运行时
#
# 目的：客户机上「系统 Python 残缺 / 没有 Python」是 SenseVoice、CT-Punc 最高频的
#      环境故障（`Failed to import encodings module`、`No module named 'sherpa_onnx'`）。
#      安装包自带一份便携运行时后，这些组件开箱即用，用户不需要 pip / 不需要配 PATH。
#
# 产物：tools/python/（约 140 MB：CPython 3.11 embeddable + numpy + sherpa-onnx）
# 用法：pwsh -File scripts/build_python_runtime.ps1 [-Force] [-PythonVersion 3.11.9]
#
# 注意：tools/ 在 .gitignore 里（除 punc_runner.py），所以该目录**不进版本库**，
#      由本脚本在打包机 / CI 上现场生成；发布工作流在编译安装包之前会先跑它。

param(
    [string]$PythonVersion = "3.11.9",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$dst = Join-Path $root "tools\python"
$pythonExe = Join-Path $dst "python.exe"

if ((Test-Path $pythonExe) -and -not $Force) {
    Write-Host "已存在内置 Python 运行时：$dst（加 -Force 可重建）"
    exit 0
}

if (Test-Path $dst) { Remove-Item -LiteralPath $dst -Recurse -Force }
New-Item -ItemType Directory -Path $dst -Force | Out-Null

$zip = Join-Path $env:TEMP "python-$PythonVersion-embed-amd64.zip"
$url = "https://www.python.org/ftp/python/$PythonVersion/python-$PythonVersion-embed-amd64.zip"
Write-Host "下载 $url"
Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing -TimeoutSec 300
Expand-Archive -LiteralPath $zip -DestinationPath $dst -Force

# embeddable 版默认不启用 site：不放开就装不了第三方包，也找不到 Lib\site-packages。
New-Item -ItemType Directory -Path (Join-Path $dst "Lib\site-packages") -Force | Out-Null
$pth = Get-ChildItem -LiteralPath $dst -Filter "python*._pth" | Select-Object -First 1
if (-not $pth) { throw "找不到 python*._pth" }
$text = [System.IO.File]::ReadAllText($pth.FullName)
$text = $text.Replace("#import site", "import site")
if ($text -notmatch "Lib\\site-packages") {
    $text = $text.TrimEnd() + "`r`nLib\site-packages`r`n"
}
[System.IO.File]::WriteAllText($pth.FullName, $text)

# pip
$getpip = Join-Path $env:TEMP "get-pip.py"
Invoke-WebRequest -Uri "https://bootstrap.pypa.io/get-pip.py" -OutFile $getpip -UseBasicParsing -TimeoutSec 300
& $pythonExe $getpip --no-warn-script-location

# 运行器实际需要的两个包（CT-Punc 与 SenseVoice 都走 sherpa-onnx）。
& $pythonExe -m pip install --no-warn-script-location --disable-pip-version-check numpy sherpa-onnx

# 自检：两个运行器都要能起来（--help 会在 import 阶段加载 numpy/sherpa_onnx）。
& $pythonExe -c "import numpy, sherpa_onnx; print('bundled python ok')"
& $pythonExe (Join-Path $root "tools\sensevoice_runner.py") --help | Out-Null
& $pythonExe (Join-Path $root "tools\punc_runner.py") --help | Out-Null

$size = (Get-ChildItem -LiteralPath $dst -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
Write-Host ("内置 Python 运行时就绪：{0}（{1:N1} MB）" -f $dst, $size)

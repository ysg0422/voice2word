# 不下载模型或启动外部程序，验证发布预取的目录布局与重名保护。
$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    (Join-Path $PSScriptRoot 'prefetch_inference_backends.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
$function = $ast.Find({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Expand-ZipFlat'
}, $false)
if (-not $function) { throw '缺少 Expand-ZipFlat' }
Invoke-Expression $function.Extent.Text
$taskRoot = Join-Path ([IO.Path]::GetTempPath()) ('v2w-release-test-' + [guid]::NewGuid().ToString('N'))
foreach ($layout in @('Release', 'outer\inner', '.')) {
    $source = Join-Path $taskRoot ('source-' + [guid]::NewGuid().ToString('N'))
    $wrapped = Join-Path $source $layout
    $null = New-Item -ItemType Directory -Path $wrapped -Force
    [IO.File]::WriteAllText((Join-Path $wrapped 'whisper-cli.exe'), 'fixture-exe')
    [IO.File]::WriteAllText((Join-Path $wrapped 'whisper.dll'), 'fixture-dll')
    $zip = "$source.zip"
    Compress-Archive -Path (Join-Path $source '*') -DestinationPath $zip
    $dest = "$source-out"
    Expand-ZipFlat -ZipPath $zip -DestDir $dest
    if ([IO.File]::ReadAllText((Join-Path $dest 'whisper-cli.exe')) -ne 'fixture-exe' -or
        [IO.File]::ReadAllText((Join-Path $dest 'whisper.dll')) -ne 'fixture-dll') {
        throw "发布组件未正确摊平：$layout"
    }
}
$source = Join-Path $taskRoot 'duplicates'
$null = New-Item -ItemType Directory -Path (Join-Path $source 'a'), (Join-Path $source 'b') -Force
[IO.File]::WriteAllText((Join-Path $source 'a\same.dll'), 'first')
[IO.File]::WriteAllText((Join-Path $source 'b\same.dll'), 'second')
$zip = "$source.zip"
Compress-Archive -Path (Join-Path $source '*') -DestinationPath $zip
$blocked = $false
try { Expand-ZipFlat -ZipPath $zip -DestDir "$source-out" } catch {
    if ($_.Exception.Message -notmatch '同名组件') { throw }
    $blocked = $true
}
if (-not $blocked) { throw '同名 DLL 必须拒绝安装' }
Write-Host 'PASS: flat, Release/, nested layouts and duplicate DLL protection'

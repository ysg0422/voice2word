<#
.SYNOPSIS
  Bench 脚本共用的路径解析：优先读 config.local.toml，其次 config.toml，最后回退内置默认。

.DESCRIPTION
  这些基准脚本原先各自硬编码 `A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe`，于是仓库里
  有 9 份「只对某台机器成立」的绝对路径——换台机器 clone 下来全部报「缺少测试文件」。

  改为统一走配置解析，与本程序的加载顺序保持一致：
    1. config.local.toml（本机专属，不进版本库）
    2. config.toml（共享配置）
    3. 内置可移植默认（tools/ffmpeg.exe 等）

  用法（在脚本顶部）：
    . (Join-Path $PSScriptRoot 'resolve_paths.ps1')
    $ffmpeg = Get-V2wPath -Key 'ffmpeg' -ProjectRoot $projectRoot
#>

function Get-V2wPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string]$Key,
        [string]$ProjectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
    )

    # 与 config.rs 同一套默认值：相对项目根，可随仓库分发
    $fallback = @{
        'ffmpeg'            = 'tools\ffmpeg.exe'
        'whisper_cli'       = 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
        'whisper_model'     = 'models\whisper\ggml-small-q5_0.bin'
        'vad_model'         = 'models\whisper\ggml-silero-v6.2.0.bin'
        'llama_cli'         = 'tools\llama-completion.exe'
        'llm_model'         = 'models\llm\qwen2.5-0.5b-instruct-q4_k_m.gguf'
    }

    $value = $null
    foreach ($name in @('config.local.toml', 'config.toml')) {
        $cfg = Join-Path $ProjectRoot $name
        if (-not (Test-Path -LiteralPath $cfg)) { continue }
        # 极简 TOML 取值：够用即可，不引入解析依赖。
        # 匹配 `key = '...'` 或 `key = "..."`，取最后一次出现（后面的段落覆盖前面的）。
        $pattern = "(?m)^\s*$([regex]::Escape($Key))\s*=\s*['""]([^'""]+)['""]"
        # 注意不要把这个结果存进 `$matches`：那是 PowerShell 的自动变量（由 -match 填充），
        # 赋值给它不会生效，函数会静默回退到默认路径。
        $found = [regex]::Matches((Get-Content -LiteralPath $cfg -Raw -Encoding UTF8), $pattern)
        if ($found.Count -gt 0) {
            $value = $found[$found.Count - 1].Groups[1].Value
            break
        }
    }

    if (-not $value) {
        if (-not $fallback.ContainsKey($Key)) { throw "未知路径键: $Key" }
        $value = $fallback[$Key]
    }

    if ([System.IO.Path]::IsPathRooted($value)) { return $value }
    return (Join-Path $ProjectRoot $value)
}
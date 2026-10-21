# 预取推理后端二进制（llama.cpp / whisper.cpp）及其同目录 DLL。
#
# 目的：让**安装包**带上完整的可运行组件，而不只是入口 exe。
#
# 为什么这一步是关键（而不是「可选优化」）：
#   - llama.cpp 的 llama-completion.exe 只有 ~9 KB，真正的代码在同目录的
#     llama-common.dll / llama-server-impl.dll / ggml-*.dll 里；
#   - whisper.cpp 的 whisper-cli.exe 同理，依赖 whisper.dll / ggml*.dll / SDL2.dll；
#   - 只发 exe 不发 DLL 时，这两个程序在客户机上会立刻以 0xC0000139
#     (STATUS_ENTRYPOINT_NOT_FOUND) 退出，**且没有任何 stderr**——
#     用户只看到「点了没反应」，日志里也查不到任何线索。
#
# 开发机上这些 DLL 就在仓库的 tools/ 下（本机跑过），所以本地怎么测都正常；
# 打包时漏掉就是「本机好使、别人电脑不好使」最典型的一种成因。
#
# 用法：pwsh -File scripts/prefetch_inference_backends.ps1 [-Force]
#
# 落盘结构必须与 config.toml 的默认路径一致：
#   tools/llama-completion.exe + tools/*.dll
#   tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe + 同目录 DLL

param(
    [switch]$Force,
    [int]$TimeoutSec = 300
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$tools = Join-Path $root "tools"
New-Item -ItemType Directory -Path $tools -Force | Out-Null

function Get-RemoteFile {
    param(
        [Parameter(Mandatory = $true)][string]$Url,
        [Parameter(Mandatory = $true)][string]$OutFile
    )
    Write-Host "下载 $Url"
    Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -TimeoutSec $TimeoutSec
}

function Expand-ZipTo {
    param(
        [Parameter(Mandatory = $true)][string]$ZipPath,
        [Parameter(Mandatory = $true)][string]$DestDir
    )
    New-Item -ItemType Directory -Path $DestDir -Force | Out-Null
    $tmp = Join-Path $env:TEMP ("v2w_unpack_" + [guid]::NewGuid().ToString("N"))
    try {
        Expand-Archive -LiteralPath $ZipPath -DestinationPath $tmp -Force
        # zip 内可能是扁平的，也可能是 Release/ 一层包装：都摊平到目标目录。
        $files = Get-ChildItem -LiteralPath $tmp -Recurse -File
        $nested = $files | Where-Object { $_.DirectoryName -ne $tmp }
        foreach ($f in $files) {
            $rel = $f.FullName.Substring($tmp.Length).TrimStart('\','/')
            # 剥掉顶层那一层包装目录（Release/、llama.cpp-b9637-win-cpu-x64/ …）。
            $parts = $rel -split '[\\/]'
            if ($parts.Count -gt 1) { $rel = ($parts[1..($parts.Count - 1)] -join '\') }
            $target = Join-Path $DestDir $rel
            $targetDir = Split-Path -Parent $target
            if ($targetDir -and -not (Test-Path $targetDir)) {
                New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
            }
            Copy-Item -LiteralPath $f.FullName -Destination $target -Force
        }
        Write-Host ("解压完成：{0} 个文件 -> {1}" -f $files.Count, $DestDir)
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Test-Complete {
    param(
        [Parameter(Mandatory = $true)][string]$EntryExe,
        [Parameter(Mandatory = $true)][string[]]$RequiredDlls,
        [Parameter(Mandatory = $true)][string]$DllDir
    )
    if (-not (Test-Path $EntryExe)) { return $false }
    foreach ($dll in $RequiredDlls) {
        if (-not (Test-Path (Join-Path $DllDir $dll))) { return $false }
    }
    return $true
}

function Test-IsVulkanOnlyBuild {
    param([Parameter(Mandatory = $true)][string]$DllDir)
    # Vulkan 版依赖 vulkan-1.dll（显卡驱动提供）——虚拟机 / 服务器 / 禁用了驱动的
    # 环境没有这个 DLL，whisper-cli 加载 ggml-vulkan.dll 时就会失败。
    # 官方 CPU 版不会有 ggml-vulkan.dll。
    return (Test-Path (Join-Path $DllDir "ggml-vulkan.dll")) -and
           (-not (Test-Path (Join-Path $DllDir "ggml-cpu.dll")))
}

# ---------------------------------------------------------------------
# FFmpeg 三件套（ffmpeg / ffplay / ffprobe）+ 它们自带的 DLL
# ---------------------------------------------------------------------
#
# 为什么必须整包取：官方发布有两种变体——
#   * static：每个 exe 自包含（体积大、零依赖）；
#   * shared：exe 很小，但依赖同目录的 avcodec / avformat / avutil / avdevice /
#     avfilter / swresample / swscale 这 7 个 DLL。
#
# 只挑 ffplay.exe 发出去、不发那 7 个 DLL，客户机上 ffplay 会以 0xC0000135
# （STATUS_DLL_NOT_FOUND）秒退——而 video_player 只会「静默跳过音频管线」，
# 用户看到的是「视频能播但没声音」。开发机上那些 DLL 恰好在 PATH 上，
# 所以本地永远正常，这就是典型的「本机好使、别人电脑不好使」。
$ffmpegExe = Join-Path $tools "ffmpeg.exe"
$ffplayExe = Join-Path $tools "ffplay.exe"
$ffprobeExe = Join-Path $tools "ffprobe.exe"

# 【关键】在**隔离的 PATH** 下试跑。
#
# 直接用当前 PATH 试跑会骗自己：本机 `A:\cppsoft\ffmpeg-6.9\bin` 在 PATH 上时，
# `tools/ffplay.exe`（shared 构建）能找齐那 7 个 DLL，测试通过——但它孤零零地
# 打进安装包后在客户机上必然 0xC0000135。
# 所以这里把 PATH 收窄到「exe 自己所在目录 + System32」，模拟客户机环境。
function Test-ExeRuns {
    param([Parameter(Mandatory = $true)][string]$Exe)
    if (-not (Test-Path $Exe)) { return $false }
    $dir = Split-Path -Parent $Exe
    $origPath = $env:PATH
    $env:PATH = "$dir;$env:SystemRoot\System32"
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $Exe
        $psi.Arguments = '-version'
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true
        $proc = [System.Diagnostics.Process]::Start($psi)
        $null = $proc.StandardOutput.ReadToEnd()
        $null = $proc.StandardError.ReadToEnd()
        $proc.WaitForExit(30000) | Out-Null
        return ($proc.ExitCode -eq 0)
    } catch {
        # 起不来说明缺 DLL（0xC0000135）或缺其它依赖——这正是在打包机上就该挡下的问题。
        return $false
    } finally {
        $env:PATH = $origPath
    }
}

# 从某个「完整 ffmpeg bin 目录」把三件套 + av*/sw* DLL 拷进 tools/。
function Copy-FfmpegFrom {
    param([Parameter(Mandatory = $true)][string]$BinDir)
    foreach ($f in @('ffmpeg.exe', 'ffplay.exe', 'ffprobe.exe')) {
        $src = Join-Path $BinDir $f
        if (-not (Test-Path $src)) { return $false }
    }
    foreach ($f in @('ffmpeg.exe', 'ffplay.exe', 'ffprobe.exe')) {
        Copy-Item -LiteralPath (Join-Path $BinDir $f) -Destination (Join-Path $tools $f) -Force
    }
    # shared 构建的 DLL 一起带上（static 构建时这两个 glob 什么都不匹配，无副作用）。
    foreach ($pattern in @('av*.dll', 'sw*.dll')) {
        Get-ChildItem -LiteralPath $BinDir -Filter $pattern -File -ErrorAction SilentlyContinue |
            ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $tools $_.Name) -Force }
    }
    return $true
}

$trioOk = (Test-ExeRuns $ffmpegExe) -and (Test-ExeRuns $ffplayExe) -and (Test-ExeRuns $ffprobeExe)
if ($Force -or -not $trioOk) {
    # 来源 1：打包机上**已装好的完整 ffmpeg**（最快、也最贴近「本机能用」的实际环境）。
    #
    # 这条路子很重要：维护者的机器上通常已经有一份能跑的 ffmpeg，
    # 把它整目录搬进 tools/ 比重新下载 200MB 快得多，而且结果一定可运行。
    $localDirs = @()
    $probe = Get-Command ffprobe.exe -ErrorAction SilentlyContinue
    if ($probe) { $localDirs += (Split-Path -Parent $probe.Source) }
    $ff = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
    if ($ff) { $localDirs += (Split-Path -Parent $ff.Source) }
    foreach ($d in @('A:\cppsoft\ffmpeg-6.9\bin', 'C:\ffmpeg\bin', 'C:\Program Files\ffmpeg\bin')) {
        if (Test-Path $d) { $localDirs += $d }
    }

    $satisfied = $false
    foreach ($d in ($localDirs | Select-Object -Unique)) {
        if (Copy-FfmpegFrom -BinDir $d) {
            if ((Test-ExeRuns $ffmpegExe) -and (Test-ExeRuns $ffplayExe) -and (Test-ExeRuns $ffprobeExe)) {
                Write-Host "FFmpeg 三件套已从本机完整安装拷入：$d"
                $satisfied = $true
                break
            }
            Write-Warning "$d 拷过来后仍跑不起来（可能只拷了 exe 没拷 DLL），继续找下一个源"
        }
    }

    # 来源 2：从上游整包下载（static 变体最稳；shared 变体则连 DLL 一起摊平）。
    if (-not $satisfied) {
        $zip = Join-Path $env:TEMP "v2w_ffmpeg.zip"
        $ffmpegUrls = @(
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl-shared.zip"
        )
        foreach ($url in $ffmpegUrls) {
            try {
                Get-RemoteFile -Url $url -OutFile $zip
                Expand-ZipTo -ZipPath $zip -DestDir $tools
                if ((Test-ExeRuns $ffmpegExe) -and (Test-ExeRuns $ffplayExe) -and (Test-ExeRuns $ffprobeExe)) {
                    $satisfied = $true
                    break
                }
                Write-Warning "$url 解出来的三件套在隔离 PATH 下跑不起来，换下一个源"
            } catch {
                Write-Warning "下载 $url 失败：$($_.Exception.Message)"
            } finally {
                Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
            }
        }
    }

    if (-not $satisfied) {
        Write-Warning "FFmpeg 三件套在隔离 PATH 下不可用：预览会没有声音、时长探测会退化"
    }
}
# ---------------------------------------------------------------------
# VC++ 可再发行运行库（VCRUNTIME140 / MSVCP140 / VCOMP140 …）
# ---------------------------------------------------------------------
# whisper-cli、whisper.dll、ggml-cpu.dll、llama-completion.exe 都依赖它们。
# Windows 10/11 大多自带，但**任何没装 VC++ Redist 的干净系统**（或企业镜像
# 精简过的系统）上，这些程序会以 0xC0000139 秒退，且没有任何 stderr。
#
# 直接放到 exe 同目录即可，零安装摩擦（微软允许再分发这些 DLL）。
$vcRuntimeDlls = @(
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "msvcp140.dll",
    "vcomp140.dll"
)
foreach ($dll in $vcRuntimeDlls) {
    $dest = Join-Path $tools $dll
    if (Test-Path $dest) { continue }

    # 来源 1：内置便携 Python 目录（它自带 vcruntime140 / vcruntime140_1）。
    # 来源 2：System32（任何装过 VC++ Redist 的打包机都有完整的四个）。
    #
    # 为什么需要来源 2：`msvcp140.dll` 与 `vcomp140.dll` **不在** python 目录里，
    # 而 whisper-cli / whisper.dll / ggml-cpu.dll 正需要它们。只查 python 目录的话
    # 打包机会「找不到」然后静默跳过——干净系统上就还是 0xC0000139。
    $candidates = @(
        (Join-Path (Join-Path $tools "python") $dll),
        (Join-Path $env:SystemRoot ("System32\" + $dll))
    )
    $copied = $false
    foreach ($src in $candidates) {
        if (Test-Path $src) {
            Copy-Item -LiteralPath $src -Destination $dest -Force
            Write-Host "复制 VC++ 运行库：$dll（来自 $src）"
            $copied = $true
            break
        }
    }
    if (-not $copied) {
        Write-Warning "缺少 VC++ 运行库 $dll（干净系统上相关程序会以 0xC0000139 秒退）"
    }
}

# ---------------------------------------------------------------------
# llama.cpp（离线翻译 / 润色）
# ---------------------------------------------------------------------
$llamaExe = Join-Path $tools "llama-completion.exe"
$llamaDlls = @("llama.dll", "llama-common.dll", "llama-server-impl.dll", "llama-completion-impl.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu-x64.dll")
if ($Force -or -not (Test-Complete -EntryExe $llamaExe -RequiredDlls $llamaDlls -DllDir $tools)) {
    $zip = Join-Path $env:TEMP "v2w_llama_cpp.zip"
    try {
        Get-RemoteFile -Url "https://hf-mirror.com/limnmn/llama.cpp-b9637-Windows-Runtime/resolve/main/llama-b9637-bin-win-cpu-x64.zip" -OutFile $zip
        # 同样先清掉开发机遗留的 llama*/ggml* DLL（自编译版本文件名相同、内容不同）。
        Get-ChildItem -LiteralPath $tools -Filter 'llama*' -File -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
        Get-ChildItem -LiteralPath $tools -Filter 'ggml*' -File -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
        Expand-ZipTo -ZipPath $zip -DestDir $tools
    } catch {
        Write-Warning "llama.cpp 预取失败（安装包将不带离线翻译组件，客户机需在界面内一键下载）：$($_.Exception.Message)"
    } finally {
        Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------
# whisper.cpp（Whisper 引擎识别）
# ---------------------------------------------------------------------
$whisperDir = Join-Path $tools "whisper-vulkan\whisper-1.8.4-windows-x64"
$whisperExe = Join-Path $whisperDir "whisper-cli.exe"
$whisperDlls = @("whisper.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll", "SDL2.dll")
# 注意：这里**必须**用官方 CPU 构建，不能沿用开发机 `tools/` 下那份自编译 Vulkan 版。
#
# 理由：开发机目录名叫 whisper-vulkan，里面装的是自己用 MSVC+Vulkan 编译的版本，
# 它的 ggml-vulkan.dll 依赖 `vulkan-1.dll`（由显卡驱动提供）——虚拟机 / 服务器 /
# 禁用了驱动的环境没有这个 DLL，whisper-cli 加载它就会失败。
#
# 更糟的是：两边文件名相同（whisper-cli.exe / ggml.dll / …），打包结果取决于
# 「谁先跑的脚本」——这就是「本机好使、别人电脑不好使」的教科书案例。
# 官方 CPU 构建不依赖 Vulkan，任何机器都能跑；想要 GPU 加速的用户可以自己跑
# scripts/build_whisper_msvc_vulkan.bat（安装包会尊重用户自编译的构建，不覆盖）。
$whisperIncomplete = -not (Test-Complete -EntryExe $whisperExe -RequiredDlls $whisperDlls -DllDir $whisperDir)
if (Test-IsVulkanOnlyBuild -DllDir $whisperDir) {
    Write-Host "检测到目录里只有自编译的 Vulkan 构建（缺 ggml-cpu.dll），改用官方 CPU 构建"
    $whisperIncomplete = $true
}
if ($Force -or $whisperIncomplete) {
    $zip = Join-Path $env:TEMP "v2w_whisper.zip"
    try {
        Get-RemoteFile -Url "https://gh-proxy.com/https://github.com/ggml-org/whisper.cpp/releases/download/v1.8.4/whisper-bin-x64.zip" -OutFile $zip
        # **先清空目标目录**：开发机上那份自编译 Vulkan 版会残留同名文件
        # （ggml-vulkan.dll 等），混进包里就等于「包内容取决于打包机的历史状态」。
        if (Test-Path $whisperDir) {
            Remove-Item -LiteralPath $whisperDir -Recurse -Force
        }
        Expand-ZipTo -ZipPath $zip -DestDir $whisperDir
    } catch {
        Write-Warning "whisper.cpp 预取失败（安装包将不带 Whisper 组件，客户机需在界面内一键下载）：$($_.Exception.Message)"
    } finally {
        Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------
# 自检：把「缺 DLL」这类问题在**打包机**上就暴露出来，而不是留到客户机。
# ---------------------------------------------------------------------
$problems = @()

# FFmpeg 三件套：不只看文件在不在，要看**能不能真的启动**。
foreach ($pair in @(
        @{ Exe = $ffmpegExe; Name = "ffmpeg" },
        @{ Exe = $ffplayExe; Name = "ffplay" },
        @{ Exe = $ffprobeExe; Name = "ffprobe" }
    )) {
    if (-not (Test-ExeRuns $pair.Exe)) {
        $problems += "$($pair.Name) 无法启动（缺同目录 DLL 或文件缺失）"
    }
}

# VC++ 运行库：whisper / llama 都依赖，缺了就是 0xC0000139。
foreach ($dll in $vcRuntimeDlls) {
    if (-not (Test-Path (Join-Path $tools $dll))) {
        $problems += "缺 VC++ 运行库 $dll"
    }
}

if (-not (Test-Complete -EntryExe $llamaExe -RequiredDlls $llamaDlls -DllDir $tools)) {
    $problems += "llama.cpp 不完整（缺 $llamaExe 或其同目录 DLL）"
}
if (-not (Test-Complete -EntryExe $whisperExe -RequiredDlls $whisperDlls -DllDir $whisperDir)) {
    $problems += "whisper.cpp 不完整（缺 $whisperExe 或其同目录 DLL）"
}
if ($problems.Count -gt 0) {
    Write-Warning "打包自检未通过，以下组件在客户机上会出问题："
    foreach ($p in $problems) { Write-Warning "  - $p" }
    Write-Warning "请先修复再打包；否则发出去的包在这些机器上必然失败。"
    # 用非零退出码让 CI / 打包脚本能感知（release.yml 已去掉 continue-on-error）。
    exit 1
} else {
    Write-Host ""
    Write-Host "打包自检通过：" -ForegroundColor Green
    Write-Host "  ffmpeg / ffplay / ffprobe  可独立启动"
    Write-Host "  llama.cpp                 入口 exe + 同目录 DLL 齐备"
    Write-Host "  whisper.cpp               入口 exe + 同目录 DLL 齐备"
    Write-Host "  VC++ 运行库                vcruntime140 / vcruntime140_1 / msvcp140 / vcomp140"
}
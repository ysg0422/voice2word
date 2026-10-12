#Requires -Version 5.1
<#
.SYNOPSIS
    Voice2Word headless transcription wrapper (Claude Code / Codex Skill helper).

.DESCRIPTION
    Transcribes an audio/video file into subtitles (srt / ass / vtt / json / txt /
    ttml / ttal) by wrapping the voice2word CLI.

    Stream contract:
      - progress and diagnostics from the CLI go to stderr and are passed through
        to this script's stderr unchanged;
      - the CLI's stdout is captured, echoed, and the result path is printed last.

    Exit codes are passed through from the CLI: 0 = success, 1 = runtime failure,
    2 = argument error. Pre-flight validation failures inside this script also exit 2.

    ENCODING NOTE: this file is intentionally ASCII-only. Windows PowerShell 5.1
    decodes a BOM-less .ps1 file using the system ANSI code page, so non-ASCII
    literals would be corrupted; keeping the source ASCII makes the script run on
    both Windows PowerShell 5.1 and PowerShell 7+.

.PARAMETER Input
    Required. Path to the audio/video file to transcribe.

.PARAMETER Output
    Optional. Output subtitle path. When omitted the CLI decides (same directory
    and base name as the input, with the extension of the chosen format).

.PARAMETER Format
    Optional. One of srt / ass / vtt / json / txt / ttml / ttal (default srt).

.PARAMETER Language
    Optional. Source language code, e.g. zh / en / ja / auto.

.PARAMETER Model
    Optional. Overrides the Whisper model file (config.toml whisper_model).
    Useful when config.toml points at a model the CLI cannot load.

.PARAMETER Translate
    Optional. Target language for translation. Note: the CLI currently only
    records this value; the translation stage is not wired up yet.

.PARAMETER Threads
    Optional. Inference thread count; must be a positive integer.

.PARAMETER Polish
    Optional switch. Force the polish / punctuation stage on.

.PARAMETER Exe
    Optional. Explicit path to voice2word.exe; highest priority.

.EXAMPLE
    .\transcribe.ps1 -Input talk.mp4 -Format vtt -Language zh -Threads 8

.EXAMPLE
    .\transcribe.ps1 -Input talk.mp4 -Output out\talk.srt -Polish

.EXAMPLE
    .\transcribe.ps1 -Input talk.mp4 -Format vtt -Model models\whisper\ggml-base.bin

.EXAMPLE
    .\transcribe.ps1 -Input talk.mp4 -Exe ..\..\..\target\release\voice2word.exe
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0, HelpMessage = 'Path to the audio/video file to transcribe')]
    [string]$Input,

    [string]$Output,
    [string]$Format,
    [string]$Language,
    [string]$Model,
    [string]$Translate,
    [string]$Threads,
    [switch]$Polish,
    [string]$Exe
)

$ErrorActionPreference = 'Stop'

# Older PowerShell has no such preference variable; disabling it explicitly keeps
# native stderr from being turned into a terminating error.
if (Get-Variable -Name 'PSNativeCommandUseErrorActionPreference' -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

# The CLI writes UTF-8; align the console encodings so text is not garbled.
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }
try { $OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }

$SupportedFormats = @('srt', 'ass', 'vtt', 'json', 'txt', 'ttml', 'ttal')

# `$Input` is a PowerShell automatic variable (the pipeline input enumerator), and
# the engine overwrites it once the script body starts. Read the bound value from
# `$PSBoundParameters` instead of trusting the variable itself.
$inputFile = $PSBoundParameters['Input']

function Stop-WithError {
    param(
        [Parameter(Mandatory = $true)][string]$Message,
        [int]$Code = 2
    )
    # Emit exactly one clean line on stderr and stop with the given exit code.
    # [Console]::Error is used instead of Write-Error because Write-Error adds a
    # multi-line "At <script>:<line> + <source excerpt>" framing in Windows
    # PowerShell 5.1, which buries the actual message in noise for any caller
    # that captures stderr.
    [Console]::Error.WriteLine($Message)
    exit $Code
}

function Resolve-Voice2WordExe {
    param(
        [string]$Explicit,
        [string]$RepoRoot
    )

    # 1) explicit -Exe
    if (-not [string]::IsNullOrWhiteSpace($Explicit)) {
        if (Test-Path -LiteralPath $Explicit -PathType Leaf) {
            return (Resolve-Path -LiteralPath $Explicit).ProviderPath
        }
        $asCommand = Get-Command -Name $Explicit -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($asCommand) { return $asCommand.Source }
        Stop-WithError -Message ("-Exe points to a file that does not exist: " + $Explicit + ". Pass a real path or omit -Exe to let the script locate the executable.")
    }

    # 2) environment variable VOICE2WORD_EXE
    $envExe = $env:VOICE2WORD_EXE
    if (-not [string]::IsNullOrWhiteSpace($envExe)) {
        if (Test-Path -LiteralPath $envExe -PathType Leaf) {
            return (Resolve-Path -LiteralPath $envExe).ProviderPath
        }
        Write-Warning ("VOICE2WORD_EXE points to a missing file, ignoring it: " + $envExe)
    }

    # 3) build artifacts inside the repository (this script lives in
    #    integrations\voice2word-skill\scripts\, so the root is three levels up)
    if (-not [string]::IsNullOrWhiteSpace($RepoRoot)) {
        $candidates = @(
            (Join-Path $RepoRoot 'target\release\voice2word.exe'),
            (Join-Path $RepoRoot 'target\debug\voice2word.exe'),
            (Join-Path $RepoRoot 'voice2word.exe')
        )
        foreach ($candidate in $candidates) {
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                return (Resolve-Path -LiteralPath $candidate).ProviderPath
            }
        }
    }

    # 4) voice2word on PATH
    $onPath = Get-Command -Name 'voice2word' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($onPath) { return $onPath.Source }

    $hint = @(
        'Cannot find voice2word.exe. Looked at: -Exe, VOICE2WORD_EXE, target\release, target\debug, repo root, PATH.',
        'Provide the executable in one of these ways:',
        '  1) -Exe <path>              e.g. -Exe .\target\release\voice2word.exe',
        '  2) VOICE2WORD_EXE env var   e.g. $env:VOICE2WORD_EXE = "C:\tools\voice2word.exe"',
        '  3) build it in the repo     cargo build --release  ->  target\release\voice2word.exe',
        '  4) put the build output directory on PATH'
    ) -join [Environment]::NewLine
    Stop-WithError -Message $hint
}

# ---- 1) validate -Input: must exist and be a regular file ----
if ([string]::IsNullOrWhiteSpace($inputFile)) {
    Stop-WithError -Message '-Input is empty: pass the path of the audio/video file to transcribe.'
}
if (-not (Test-Path -LiteralPath $inputFile)) {
    Stop-WithError -Message ("Input file not found: " + $inputFile + ". Check the path and file name; relative paths are resolved against the current directory " + (Get-Location).Path + ".")
}
$inputPath = (Resolve-Path -LiteralPath $inputFile).ProviderPath
if (-not (Test-Path -LiteralPath $inputPath -PathType Leaf)) {
    Stop-WithError -Message ("Input path is not a file: " + $inputFile + ". A directory or device cannot be transcribed; point at a concrete media file.")
}

# ---- 2) validate -Format: must be one of the supported formats ----
$formatValue = $null
if ($PSBoundParameters.ContainsKey('Format') -and -not [string]::IsNullOrWhiteSpace($Format)) {
    $formatValue = $Format.Trim().ToLowerInvariant()
    if ($SupportedFormats -notcontains $formatValue) {
        Stop-WithError -Message ("Invalid -Format: " + $Format + ". Supported values: " + ($SupportedFormats -join ' / ') + ".")
    }
}

# ---- 3) validate -Model: when provided it must point at an existing file ----
$modelValue = $null
if ($PSBoundParameters.ContainsKey('Model') -and -not [string]::IsNullOrWhiteSpace($Model)) {
    if (-not (Test-Path -LiteralPath $Model -PathType Leaf)) {
        Stop-WithError -Message ("Invalid -Model: file not found: " + $Model + ". Pass an existing model file or omit -Model to use config.toml.")
    }
    $modelValue = (Resolve-Path -LiteralPath $Model).ProviderPath
}

# ---- 4) validate -Threads: when provided it must be a positive integer ----
$threadsValue = $null
if ($PSBoundParameters.ContainsKey('Threads') -and -not [string]::IsNullOrWhiteSpace($Threads)) {
    $parsed = 0
    if (-not [int]::TryParse($Threads.Trim(), [ref]$parsed)) {
        Stop-WithError -Message ("Invalid -Threads: " + $Threads + ". A positive integer is required, e.g. -Threads 8.")
    }
    if ($parsed -le 0) {
        Stop-WithError -Message ("Invalid -Threads: " + $Threads + ". The value must be greater than 0.")
    }
    $threadsValue = $parsed
}

# ---- 5) locate the executable ----
$repoRoot = $null
try {
    $repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..\..') -ErrorAction Stop).ProviderPath
} catch {
    $repoRoot = $null
}
$exePath = Resolve-Voice2WordExe -Explicit $Exe -RepoRoot $repoRoot

# ---- 6) map parameters onto the CLI ----
$cliArgs = New-Object System.Collections.Generic.List[string]
$cliArgs.Add('transcribe')
$cliArgs.Add($inputPath)
if (-not [string]::IsNullOrWhiteSpace($Output)) {
    $cliArgs.Add('--output')
    $cliArgs.Add($Output)
}
if ($null -ne $formatValue) {
    $cliArgs.Add('--format')
    $cliArgs.Add($formatValue)
}
if (-not [string]::IsNullOrWhiteSpace($Language)) {
    $cliArgs.Add('--lang')
    $cliArgs.Add($Language)
}
if ($null -ne $modelValue) {
    $cliArgs.Add('--model')
    $cliArgs.Add($modelValue)
}
if (-not [string]::IsNullOrWhiteSpace($Translate)) {
    $cliArgs.Add('--translate')
    $cliArgs.Add($Translate)
}
if ($null -ne $threadsValue) {
    $cliArgs.Add('--threads')
    $cliArgs.Add([string]$threadsValue)
}
if ($Polish) {
    $cliArgs.Add('--polish')
}

Write-Verbose ("executable: " + $exePath)
Write-Verbose ("invoke: " + $exePath + ' ' + ($cliArgs -join ' '))

# ---- 7) call the CLI: stderr passes through, stdout is captured ----
$stdoutLines = @(& $exePath @cliArgs)
$exitCode = $LASTEXITCODE
if ($null -eq $exitCode) { $exitCode = 0 }

if ($exitCode -ne 0) {
    # On failure echo whatever the CLI put on stdout, then pass the code through.
    foreach ($line in $stdoutLines) { Write-Output $line }
    exit $exitCode
}

# ---- 8) echo stdout and print the result path ----
# The success line is "subtitle written: <path> (N segments)"; match it with \u
# escapes so the pattern itself stays ASCII.
$resultPath = $null
foreach ($line in $stdoutLines) {
    if ($line -match '^\u5b57\u5e55\u5df2\u5199\u5165:\s*(.+?)\uff08\u5171\s*\d+\s*\u6bb5\uff09\s*$') {
        $resultPath = $Matches[1].Trim()
    }
}
foreach ($line in $stdoutLines) { Write-Output $line }
if ($null -eq $resultPath) {
    # Exit code 0 without the success line means no subtitle file was confirmed.
    # The usual cause is a stale executable that predates the CLI subcommand and
    # therefore just opened its GUI window.
    Stop-WithError -Code 1 -Message ('The CLI exited 0 but never printed the expected "subtitle written" line, so no subtitle file could be confirmed. If the executable is an old build it may have opened a GUI window instead: rebuild with "cargo build --release", or pass -Exe target\debug\voice2word.exe.')
}
Write-Output ("Result path: " + $resultPath)
exit 0
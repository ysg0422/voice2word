@echo off
setlocal enabledelayedexpansion
rem ============================================================================
rem  build_whisper_msvc_vulkan.bat
rem
rem  用 **MSVC + Vulkan** 自行编译 whisper.cpp 的 whisper-cli（GPU 加速版），
rem  并部署到 tools/whisper-vulkan/whisper-1.8.4-windows-x64/。
rem
rem  为什么推荐 MSVC 而不是 MinGW：
rem    MSVC 产物依赖 VC++ 运行库（VCRUNTIME140/MSVCP140/VCOMP140，Win10/11 大多自带），
rem    而 MinGW 产物依赖 libgcc_s_seh-1.dll / libstdc++-6.dll / libgomp-1.dll，
rem    用户机器缺一个就 0xC0000139 秒退、stderr 全空，极难排查。
rem
rem  前置条件（脚本会自检）：
rem    - Visual Studio 2019/2022，勾选「使用 C++ 的桌面开发」
rem    - CMake + Ninja（VS 自带；也可自行安装并加入 PATH）
rem    - Vulkan SDK（https://vulkan.lunarg.com/），需要 glslc.exe 编译 shader
rem    - whisper.cpp 源码（脚本会自动下载 v1.8.4 到 _whisper_build/）
rem
rem  用法：双击本脚本，或在 cmd 中运行。产物自动覆盖部署目录。
rem ============================================================================

rem 脚本位于 scripts\ 下，项目根是它的上一级。
set "ROOT=%~dp0.."
for %%i in ("%ROOT%") do set "ROOT=%%~fi"
cd /d "%ROOT%"

set "VERSION=1.8.4"
set "WORK=%ROOT%_whisper_build"
set "SRC=%WORK%\whisper.cpp-%VERSION%"
set "DEPLOY=%ROOT%tools\whisper-vulkan\whisper-1.8.4-windows-x64"

echo [1/5] 定位 Visual Studio (vcvars64.bat)...
set "VCVARS="
for /f "usebackq tokens=*" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find VC\Auxiliary\Build\vcvars64.bat`) do set "VCVARS=%%i"
if not defined VCVARS (
    echo   vswhere 未找到 VS；尝试常见安装路径...
    for %%p in (
        "%ProgramFiles%\Microsoft Visual Studio\2022\Community"
        "%ProgramFiles%\Microsoft Visual Studio\2022\Professional"
        "%ProgramFiles%\Microsoft Visual Studio\2022\Enterprise"
        "D:\VS Stdioh\IDE"
    ) do if exist "%%~p\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=%%~p\VC\Auxiliary\Build\vcvars64.bat"
)
if not defined VCVARS (
    echo ERROR: 找不到 vcvars64.bat。请安装 Visual Studio 并勾选「使用 C++ 的桌面开发」。
    exit /b 1
)
echo   VCVARS=%VCVARS%
call "%VCVARS%"
if errorlevel 1 ( echo ERROR: vcvars64.bat 执行失败 & exit /b 1 )

echo [2/5] 检查 Vulkan SDK (glslc.exe)...
where glslc.exe >nul 2>nul
if errorlevel 1 (
    if defined VULKAN_SDK (
        set "PATH=%VULKAN_SDK%\Bin;%PATH%"
    ) else (
        echo ERROR: 未找到 glslc.exe。请安装 Vulkan SDK 并设置 VULKAN_SDK 环境变量。
        echo        下载: https://vulkan.lunarg.com/sdk/home
        exit /b 1
    )
)

echo [3/5] 准备 whisper.cpp 源码 v%VERSION% ...
if not exist "%SRC%\CMakeLists.txt" (
    if not exist "%WORK%" mkdir "%WORK%"
    echo   下载 whisper.cpp v%VERSION% 源码包...
    powershell -NoProfile -Command "$u='https://gh-proxy.com/https://github.com/ggml-org/whisper.cpp/archive/refs/tags/v%VERSION%.zip'; $o='%WORK%\whisper-src.zip'; try { Invoke-WebRequest -Uri $u -OutFile $o -UseBasicParsing } catch { Invoke-WebRequest -Uri ('https://ghproxy.net/https://github.com/ggml-org/whisper.cpp/archive/refs/tags/v%VERSION%.zip') -OutFile $o -UseBasicParsing }"
    if not exist "%WORK%\whisper-src.zip" ( echo ERROR: 源码下载失败 & exit /b 1 )
    powershell -NoProfile -Command "Expand-Archive -LiteralPath '%WORK%\whisper-src.zip' -DestinationPath '%WORK%' -Force"
    if not exist "%SRC%\CMakeLists.txt" ( echo ERROR: 解压后未找到源码目录 & exit /b 1 )
)

echo [4/5] 配置 CMake (MSVC + Vulkan + OpenMP)...
cmake -S "%SRC%" -B "%SRC%\build" -G Ninja ^
  -DCMAKE_BUILD_TYPE=Release ^
  -DGGML_VULKAN=ON ^
  -DGGML_OPENMP=ON ^
  -DWHISPER_BUILD_EXAMPLES=ON ^
  -DWHISPER_BUILD_TESTS=OFF ^
  -DWHISPER_BUILD_SERVER=OFF
if errorlevel 1 ( echo ERROR: cmake configure 失败 & exit /b 1 )

echo [5/5] 编译 whisper-cli ...
cmake --build "%SRC%\build" --config Release --target whisper-cli -j %NUMBER_OF_PROCESSORS%
if errorlevel 1 ( echo ERROR: 编译失败 & exit /b 1 )

echo 部署到 %DEPLOY% ...
if not exist "%DEPLOY%" mkdir "%DEPLOY%"
for %%f in (whisper-cli.exe whisper.dll ggml.dll ggml-cpu.dll ggml-base.dll ggml-vulkan.dll) do (
    copy /y "%SRC%\build\bin\%%f" "%DEPLOY%\%%f" >nul
)
echo.
echo ============================================================================
echo  构建完成并已部署。
echo  产物: %DEPLOY%\whisper-cli.exe
echo  验证: "%DEPLOY%\whisper-cli.exe" -m models\whisper\ggml-small-q5_0.bin -f resources\sample\test_speech.wav -l zh -t 8 -bo 1 -bs 1 -nf -mc 32 -sns -fa
echo  期望看到: whisper_backend_init_gpu: using Vulkan0 backend
echo ============================================================================
echo DONE

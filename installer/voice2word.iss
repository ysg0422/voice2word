; =============================================================================
; Voice2Word — Inno Setup 极简现代安装向导脚本
; =============================================================================

#define MyAppName "Voice2Word"
#ifndef MyAppVersion
#define MyAppVersion "0.2.26"
#endif
#define MyAppPublisher "Voice2Word"
#define MyAppURL "https://github.com/ysg0422/voice2word"
#define MyAppExeName "voice2word.exe"

[Setup]
AppId={{D37E84B1-28C9-4C5E-98BF-23F1735BC149}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
OutputDir=..\dist
OutputBaseFilename=Voice2Word-v{#MyAppVersion}-Setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
SetupIconFile=assets\app.ico
WizardImageFile=assets\wizard_side.bmp
WizardSmallImageFile=assets\wizard_small.bmp
PrivilegesRequired=lowest
; **关键**：不声明这两项时，Inno 以 32 位模式安装，卸载项会被写进
; `HKCU\Software\WOW6432Node\...\Uninstall\...`（32 位注册表视图），
; Windows 10/11 的「设置 → 应用」对**每用户安装**的 32 位视图条目经常不展示，
; 于是客户看到「装了但应用列表里找不到」。声明 64 位安装后条目落进 64 位视图。
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
ShowLanguageDialog=no
DisableWelcomePage=no
DisableReadyPage=yes
DisableDirPage=no
DisableProgramGroupPage=yes
DisableFinishedPage=no
UninstallDisplayIcon={app}\app.ico
UninstallDisplayName={#MyAppName}
; 显式声明卸载能力：此前只是靠 Inno 默认值，客户在「Windows 设置 → 应用」里
; 找不到本程序时无从排查。这两项保证卸载项（HKCU\...\Uninstall\{AppId}_is1）
; 一定会被写入。
Uninstallable=yes
CreateUninstallRegKey=yes
; 卸载/升级时若程序仍在运行，交给 Restart Manager 关掉它，避免文件占用导致
; 「卸不干净」；不自动重启应用。
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Dirs]
Name: "{app}\models"
Name: "{app}\tools"

[Files]
; 应用独立高清图标文件 (供快捷方式与卸载程序读取)
Source: "assets\app.ico"; DestDir: "{app}"; Flags: ignoreversion
; 欢迎页沉浸式背景大图 (只解压到临时目录供安装向导渲染，不进安装目录)
Source: "assets\wizard_bg.bmp"; Flags: dontcopy
; 核心程序与工具底座
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\voice2word-cli.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; 主程序自己的 VC++ 运行库：voice2word.exe / voice2word-cli.exe 的 PE 导入表里
; **直接**写着 vcruntime140.dll（Rust 的 MSVC 目标默认动态链接 CRT）。
; 它**不是** Windows 自带组件——干净的 Windows 上没有这个文件，
; 此前只把它放到 {app}\tools\ 里（那是给 whisper / llama 用的），
; 结果主程序在没装过 VC++ Redist 的机器上会以 0xC0000135 起不来。
; 必须放在**与 exe 同一目录**。
Source: "..\tools\vcruntime140.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\vcruntime140_1.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; 共享配置：**不打包**仓库里的 config.toml。
;
; `config.toml` 在仓库里是被跟踪的，而开发机上的取值往往是**本机专属**的
; （例如 whisper_model 指向 SenseVoice 的 onnx、并行进程数按 16 核调过）。
; 直接把它打进安装包，客户机拿到的就是一份「按别人机器调好的配置」——
; 最典型的是 `whisper_model` 指向 SenseVoice 模型：引擎选 Whisper、模型是 ONNX，
; whisper-cli 读不了这个文件，**退出码 0 且不写 JSON**，上层只能报「未产出任何识别结果」。
; 这正是 CHANGELOG 0.2.19 记录过的客户机故障。
;
; 不打包即可：程序首次启动时用**内置默认值**生成一份，那些默认值全是相对项目根的
; 便携路径（tools/ffmpeg.exe、tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe、
; models/whisper/ggml-small-q5_0.bin …），与本机选择无关；
; 用户在界面里改过的设置则存在 config.local.toml（升级时不覆盖）。
; 随包默认配置：**必须打包**，否则装在只读目录的客户机每次启动都回到默认值、
; 界面里改过的设置存不下来。
;
; 它的内容由发布流程用 `voice2word-cli --print-default-config` 现场生成——
; 也就是**代码里的默认值**，而不是仓库里那份被跟踪的开发机配置
; （那份的 whisper_model 指向 SenseVoice 的 onnx，客户机上 Whisper 引擎会
; 退出码 0 且不写 JSON）。onlyifdoesntexist：升级时不覆盖用户已改过的配置。
Source: "..\config.toml"; DestDir: "{app}"; Flags: onlyifdoesntexist skipifsourcedoesntexist
Source: "..\tools\punc_runner.py"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\sensevoice_runner.py"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
; TTS 配音运行器（与前两个同样是随包分发的胶水脚本）
Source: "..\tools\edge_tts_runner.py"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\ffmpeg.exe"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\ffplay.exe"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
; ffplay 的共享库依赖（avcodec / avformat / avutil / avdevice / avfilter / swresample / swscale）。
;
; **静态构建时这些文件不存在**，skipifsourcedoesntexist 会直接跳过，无副作用；
; 但若打包机用的是 `--enable-shared` 的 ffplay，这几条是它能在客户机上跑起来的唯一保证——
; 缺一个 DLL，ffplay 就以 0xC0000135 秒退，而代码只会「静默跳过音频」，
; 用户看到的是「视频能播但没声音」。
Source: "..\tools\av*.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\sw*.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
; ---------------------------------------------------------------------------
; 运行时 DLL（**必须**随包分发，缺了就是「本机好使、别人电脑不好使」）
;
; llama.cpp 的入口 exe 只有几 KB（真正代码在同目录 DLL），whisper.cpp 同理：
; 只发 exe 不发 DLL，客户机启动时进程会立刻以 0xC0000139（STATUS_ENTRYPOINT_NOT_FOUND）
; 退出，而且**没有任何 stderr**——用户只看到「点了没反应」，日志里也查不到线索。
;
; 开发机上 DLL 就在 tools/ 下（本仓库跑过），所以本地怎么测都正常；
; 打包时若只挑 exe，客户机就必然失败。这三条规则是本安装包最重要的部分。
; ---------------------------------------------------------------------------
; llama.cpp：入口 exe + 全部同目录 DLL（llama-common / llama-server-impl / ggml-* …）
Source: "..\tools\llama*.exe"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\llama*.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\ggml*.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\mtmd.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\libomp140.x86_64.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
; VC++ 可再发行运行库（llama-completion.exe / llama.dll 依赖 VCRUNTIME140）
Source: "..\tools\vcruntime140.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\vcruntime140_1.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\msvcp140.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\vcomp140.dll"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
; whisper.cpp（官方 CPU 构建）：入口 exe + 同目录 DLL（whisper.dll / ggml*.dll / SDL2.dll）
Source: "..\tools\whisper-vulkan\whisper-1.8.4-windows-x64\*"; DestDir: "{app}\tools\whisper-vulkan\whisper-1.8.4-windows-x64"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist
; VC++ 可再发行运行库：whisper-cli / whisper.dll / ggml-cpu.dll 都依赖
; MSVCP140 / VCRUNTIME140 / VCRUNTIME140_1 / VCOMP140。
; Windows 10/11 大多自带，但**任何没装 VC++ Redist 的干净系统**（或企业镜像
; 精简过的系统）上 whisper-cli 会以 0xC0000139 秒退，且没有任何 stderr。
; 直接把这几个 DLL 放到 exe 同目录即可，零安装摩擦（微软允许再分发）。
Source: "..\tools\vcruntime140.dll"; DestDir: "{app}\tools\whisper-vulkan\whisper-1.8.4-windows-x64"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\vcruntime140_1.dll"; DestDir: "{app}\tools\whisper-vulkan\whisper-1.8.4-windows-x64"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\msvcp140.dll"; DestDir: "{app}\tools\whisper-vulkan\whisper-1.8.4-windows-x64"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\vcomp140.dll"; DestDir: "{app}\tools\whisper-vulkan\whisper-1.8.4-windows-x64"; Flags: ignoreversion skipifsourcedoesntexist
; ffprobe：与 ffmpeg 同目录。缺失时时长探测会退回解析 `ffmpeg -i` 的 stderr（能工作，
; 但精度更差、耗时更长），因此它是**可选但强烈建议**随包分发。
Source: "..\tools\ffprobe.exe"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
; 内置便携 Python：SenseVoice / CT-Punc 的运行环境，避免客户机「系统 Python 残缺」。
; 由 scripts/build_python_runtime.ps1 现场生成（tools/ 不进版本库），缺失时跳过。
Source: "..\tools\python\*"; DestDir: "{app}\tools\python"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"; WorkingDir: "{app}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon; IconFilename: "{app}\app.ico"

[Run]
; 安装后自检：**先**把「组件是否齐全」告诉用户，再启动主程序。
;
; 为什么必须自检：llama.cpp / whisper.cpp 的入口 exe 依赖同目录 DLL，缺一个就
; 以 0xC0000139 静默退出。没有这一步，用户装完、点开、点「开始转写」、
; 程序没反应、日志里也没有线索——只能回头找我们。
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[Registry]
; 老版本（0.2.24 及以前）按 32 位视图写的卸载项：升到 64 位安装后它不会再被更新，
; 留着会让「设置 → 应用」出现两条（一条已经失效）。装新版时顺手删掉。
Root: HKCU; Subkey: "Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{{D37E84B1-28C9-4C5E-98BF-23F1735BC149}_is1"; Flags: deletekey dontcreatekey

[UninstallDelete]
; 纯缓存/日志：无论用户是否保留数据，都必须删掉，否则就是「卸不干净」。
Type: filesandordirs; Name: "{app}\logs"
Type: filesandordirs; Name: "{app}\cache"
Type: filesandordirs; Name: "{app}\updates"
; 断点续跑的阶段产物目录（名字以点开头，默认会漏掉，卸载后残留一整块数据目录）。
Type: filesandordirs; Name: "{app}\.voice2word-stages"
Type: files; Name: "{app}\*.part"

[Code]
const
  DWMWA_USE_IMMERSIVE_DARK_MODE = 20;
  DWMWA_USE_IMMERSIVE_DARK_MODE_OLD = 19;

function DwmSetWindowAttribute(hwnd: HWND; dwAttribute: DWORD; var pvAttribute: DWORD; cbAttribute: DWORD): Longint;
  external 'DwmSetWindowAttribute@dwmapi.dll stdcall delayload';
function CreateRoundRectRgn(X1, Y1, X2, Y2, X3, Y3: Integer): Longword;
  external 'CreateRoundRectRgn@gdi32.dll stdcall delayload';
function SetWindowRgn(hWnd: HWND; hRgn: Longword; bRedraw: Boolean): Integer;
  external 'SetWindowRgn@user32.dll stdcall delayload';

var
  WelcomeBgImage: TBitmapImage;
  PathContainer: TPanel;
  PathEdit: TEdit;
  PathLabel: TLabel;
  BrowseBtnPnl: TPanel;
  BrowseBtnLbl: TLabel;
  DesktopPanel: TPanel;
  DesktopCheckBox: TCheckBox;
  DesktopLabel: TLabel;
  DarkBottomBar: TPanel;
  CustomNextBtn: TPanel;
  CustomNextLbl: TLabel;
  CustomCancelBtn: TPanel;
  CustomCancelLbl: TLabel;

procedure ApplyDarkThemeToWindow();
var
  DarkMode: DWORD;
begin
  DarkMode := 1;
  if DwmSetWindowAttribute(WizardForm.Handle, DWMWA_USE_IMMERSIVE_DARK_MODE, DarkMode, SizeOf(DarkMode)) <> 0 then
    DwmSetWindowAttribute(WizardForm.Handle, DWMWA_USE_IMMERSIVE_DARK_MODE_OLD, DarkMode, SizeOf(DarkMode));
end;

procedure BrowseBtnClick(Sender: TObject);
var
  NewDir: string;
begin
  NewDir := PathEdit.Text;
  if BrowseForFolder('请选择 Voice2Word 安装路径：', NewDir, True) then
  begin
    PathEdit.Text := NewDir;
    WizardForm.DirEdit.Text := NewDir;
  end;
end;

procedure PathEditChange(Sender: TObject);
begin
  WizardForm.DirEdit.Text := PathEdit.Text;
end;

procedure DesktopLabelClick(Sender: TObject);
begin
  DesktopCheckBox.Checked := not DesktopCheckBox.Checked;
end;

procedure CustomNextBtnClick(Sender: TObject);
begin
  WizardForm.NextButton.OnClick(WizardForm.NextButton);
end;

procedure CustomCancelBtnClick(Sender: TObject);
begin
  WizardForm.CancelButton.OnClick(WizardForm.CancelButton);
end;

procedure InitializeWizard();
var
  CardW, CardH, CardLeft, CardTop: Integer;
  BottomBarH: Integer;
begin
  // 1. 启用 Windows 10/11 原生沉浸式深黑标题栏 (告别亮白顶栏)
  ApplyDarkThemeToWindow();

  // 彻底斩断边缘白线/分割线，窗体深黑 #121316 (BGR: $161312)
  WizardForm.Color := $161312;
  WizardForm.Bevel.Visible := False;
  WizardForm.Bevel1.Visible := False;
  WizardForm.WizardBitmapImage.Visible := False;
  WizardForm.WizardBitmapImage2.Visible := False;
  WizardForm.OuterNotebook.Align := alClient;
  WizardForm.InnerNotebook.Align := alClient;

  // 2. 底栏预备 (用于安装中 wpInstalling 与完成页 wpFinished)
  BottomBarH := ScaleY(56);
  DarkBottomBar := TPanel.Create(WizardForm);
  DarkBottomBar.Parent := WizardForm;
  DarkBottomBar.Left := 0;
  DarkBottomBar.Top := WizardForm.ClientHeight - BottomBarH;
  DarkBottomBar.Width := WizardForm.ClientWidth;
  DarkBottomBar.Height := BottomBarH;
  DarkBottomBar.Color := $161312;
  DarkBottomBar.ParentBackground := False;
  DarkBottomBar.BevelOuter := bvNone;
  DarkBottomBar.Visible := False; // 欢迎页隐藏死黑底座，全屏沉浸一体

  // 隐藏原生白底按钮，移出可视区域
  WizardForm.NextButton.Left := -1000;
  WizardForm.CancelButton.Left := -1000;
  WizardForm.BackButton.Left := -1000;

  // 3. 提取 iOS 极简居中美学背景大图并在欢迎页完整铺满
  ExtractTemporaryFile('wizard_bg.bmp');
  
  WelcomeBgImage := TBitmapImage.Create(WizardForm);
  WelcomeBgImage.Parent := WizardForm.WelcomePage;
  WelcomeBgImage.Bitmap.LoadFromFile(ExpandConstant('{tmp}\wizard_bg.bmp'));
  WelcomeBgImage.Stretch := True;
  WelcomeBgImage.Align := alClient;

  // 隐藏原生说明标签
  WizardForm.WelcomeLabel1.Visible := False;
  WizardForm.WelcomeLabel2.Visible := False;

  // 4. 居中排布 iOS 磨砂胶囊风格路径选择框
  CardW := ScaleX(440);
  CardH := ScaleY(38);
  CardLeft := (WizardForm.WelcomePage.Width - CardW) div 2;
  CardTop := WizardForm.WelcomePage.Height - ScaleY(152);

  // 路径小标题 (居中对齐上方)
  PathLabel := TLabel.Create(WizardForm);
  PathLabel.Parent := WizardForm.WelcomePage;
  PathLabel.Left := CardLeft + ScaleX(8);
  PathLabel.Top := CardTop - ScaleY(20);
  PathLabel.Caption := '安装路径';
  PathLabel.Font.Name := 'Segoe UI';
  PathLabel.Font.Size := 8;
  PathLabel.Font.Style := [fsBold];
  PathLabel.Font.Color := $0099D334;
  PathLabel.Transparent := True;

  // 磨砂圆角胶囊容器
  PathContainer := TPanel.Create(WizardForm);
  PathContainer.Parent := WizardForm.WelcomePage;
  PathContainer.Left := CardLeft;
  PathContainer.Top := CardTop;
  PathContainer.Width := CardW;
  PathContainer.Height := CardH;
  PathContainer.Color := $221D1A; // 深炭灰磨砂底板
  PathContainer.ParentBackground := False;
  PathContainer.BevelOuter := bvNone;
  SetWindowRgn(PathContainer.Handle, CreateRoundRectRgn(0, 0, CardW, CardH, ScaleX(16), ScaleY(16)), True);

  // 胶囊内嵌的圆角 [更改] 按钮 (像素级对称居中内嵌)
  BrowseBtnPnl := TPanel.Create(WizardForm);
  BrowseBtnPnl.Parent := PathContainer;
  BrowseBtnPnl.Width := ScaleX(58);
  BrowseBtnPnl.Height := ScaleY(26);
  BrowseBtnPnl.Left := CardW - BrowseBtnPnl.Width - ScaleX(6);
  BrowseBtnPnl.Top := (CardH - BrowseBtnPnl.Height) div 2;
  BrowseBtnPnl.Color := $322B27; // 优雅深灰微高亮底色
  BrowseBtnPnl.ParentBackground := False;
  BrowseBtnPnl.BevelOuter := bvNone;
  BrowseBtnPnl.Cursor := crHandPoint;
  BrowseBtnPnl.OnClick := @BrowseBtnClick;
  SetWindowRgn(BrowseBtnPnl.Handle, CreateRoundRectRgn(0, 0, BrowseBtnPnl.Width, BrowseBtnPnl.Height, BrowseBtnPnl.Height, BrowseBtnPnl.Height), True);

  BrowseBtnLbl := TLabel.Create(WizardForm);
  BrowseBtnLbl.Parent := BrowseBtnPnl;
  BrowseBtnLbl.Align := alClient;
  BrowseBtnLbl.Alignment := taCenter;
  BrowseBtnLbl.Layout := tlCenter;
  BrowseBtnLbl.Caption := '更改';
  BrowseBtnLbl.Font.Name := 'Segoe UI';
  BrowseBtnLbl.Font.Size := 9;
  BrowseBtnLbl.Font.Color := $00E0E0E0;
  BrowseBtnLbl.Transparent := True;
  BrowseBtnLbl.Cursor := crHandPoint;
  BrowseBtnLbl.OnClick := @BrowseBtnClick;

  // 输入框内嵌于胶囊槽中
  PathEdit := TEdit.Create(WizardForm);
  PathEdit.Parent := PathContainer;
  PathEdit.Left := ScaleX(16);
  PathEdit.Height := ScaleY(20);
  PathEdit.Top := (CardH - PathEdit.Height) div 2;
  PathEdit.Width := BrowseBtnPnl.Left - ScaleX(22);
  PathEdit.Font.Name := 'Segoe UI';
  PathEdit.Font.Size := 9;
  PathEdit.Color := $221D1A;
  PathEdit.Font.Color := $00FFFFFF;
  PathEdit.BorderStyle := bsNone;
  PathEdit.AutoSelect := False;
  PathEdit.Text := WizardForm.DirEdit.Text;
  if PathEdit.Text = '' then
    PathEdit.Text := ExpandConstant('{autopf}\{#MyAppName}');
  PathEdit.SelLength := 0;
  PathEdit.SelStart := Length(PathEdit.Text);
  PathEdit.OnChange := @PathEditChange;

  // 5. 居中对齐的桌面快捷方式勾选框 (完美位于中轴线上)
  DesktopPanel := TPanel.Create(WizardForm);
  DesktopPanel.Parent := WizardForm.WelcomePage;
  DesktopPanel.Width := ScaleX(170);
  DesktopPanel.Height := ScaleY(24);
  DesktopPanel.Left := (WizardForm.WelcomePage.Width - DesktopPanel.Width) div 2;
  DesktopPanel.Top := CardTop + CardH + ScaleY(10);
  DesktopPanel.Color := $121316;
  DesktopPanel.ParentBackground := False;
  DesktopPanel.BevelOuter := bvNone;

  DesktopCheckBox := TCheckBox.Create(WizardForm);
  DesktopCheckBox.Parent := DesktopPanel;
  DesktopCheckBox.Left := 0;
  DesktopCheckBox.Top := ScaleY(2);
  DesktopCheckBox.Width := ScaleX(20);
  DesktopCheckBox.Color := $121316;
  DesktopCheckBox.Checked := True;

  DesktopLabel := TLabel.Create(WizardForm);
  DesktopLabel.Parent := DesktopPanel;
  DesktopLabel.Left := ScaleX(24);
  DesktopLabel.Top := ScaleY(2);
  DesktopLabel.Caption := '创建桌面快捷方式';
  DesktopLabel.Font.Name := 'Segoe UI';
  DesktopLabel.Font.Size := 9;
  DesktopLabel.Font.Color := $00AAAAAA;
  DesktopLabel.Transparent := True;
  DesktopLabel.OnClick := @DesktopLabelClick;
  DesktopLabel.Cursor := crHandPoint;

  // 6. 居中大号 iOS 胶囊药丸主安装按钮 (Center Pill CTA Button)
  CustomNextBtn := TPanel.Create(WizardForm);
  CustomNextBtn.Parent := WizardForm.WelcomePage;
  CustomNextBtn.Width := ScaleX(280);
  CustomNextBtn.Height := ScaleY(44);
  CustomNextBtn.Left := (WizardForm.WelcomePage.Width - CustomNextBtn.Width) div 2;
  CustomNextBtn.Top := WizardForm.WelcomePage.Height - ScaleY(66);
  CustomNextBtn.Color := $0099D334; // 苹果高亮薄荷翡翠绿
  CustomNextBtn.ParentBackground := False;
  CustomNextBtn.BevelOuter := bvNone;
  CustomNextBtn.Cursor := crHandPoint;
  CustomNextBtn.OnClick := @CustomNextBtnClick;
  SetWindowRgn(CustomNextBtn.Handle, CreateRoundRectRgn(0, 0, CustomNextBtn.Width, CustomNextBtn.Height, CustomNextBtn.Height, CustomNextBtn.Height), True);
  CustomNextBtn.BringToFront;

  CustomNextLbl := TLabel.Create(WizardForm);
  CustomNextLbl.Parent := CustomNextBtn;
  CustomNextLbl.Align := alClient;
  CustomNextLbl.Alignment := taCenter;
  CustomNextLbl.Layout := tlCenter;
  CustomNextLbl.Caption := '立即安装';
  CustomNextLbl.Font.Name := 'Segoe UI';
  CustomNextLbl.Font.Size := 11;
  CustomNextLbl.Font.Style := [fsBold];
  CustomNextLbl.Font.Color := $000D0B0A; // 纯曜石黑，对比度极高
  CustomNextLbl.Transparent := True;
  CustomNextLbl.Cursor := crHandPoint;
  CustomNextLbl.OnClick := @CustomNextBtnClick;

  // [取消] 按钮预备 (安装过程中显示于底栏)
  CustomCancelBtn := TPanel.Create(WizardForm);
  CustomCancelBtn.Parent := DarkBottomBar;
  CustomCancelBtn.Width := ScaleX(86);
  CustomCancelBtn.Height := ScaleY(36);
  CustomCancelBtn.Left := DarkBottomBar.Width - ScaleX(96);
  CustomCancelBtn.Top := (BottomBarH - ScaleY(36)) div 2;
  CustomCancelBtn.Color := $24201E;
  CustomCancelBtn.ParentBackground := False;
  CustomCancelBtn.BevelOuter := bvNone;
  CustomCancelBtn.Cursor := crHandPoint;
  CustomCancelBtn.OnClick := @CustomCancelBtnClick;
  SetWindowRgn(CustomCancelBtn.Handle, CreateRoundRectRgn(0, 0, CustomCancelBtn.Width, CustomCancelBtn.Height, CustomCancelBtn.Height, CustomCancelBtn.Height), True);

  CustomCancelLbl := TLabel.Create(WizardForm);
  CustomCancelLbl.Parent := CustomCancelBtn;
  CustomCancelLbl.Align := alClient;
  CustomCancelLbl.Alignment := taCenter;
  CustomCancelLbl.Layout := tlCenter;
  CustomCancelLbl.Caption := '取消';
  CustomCancelLbl.Font.Name := 'Segoe UI';
  CustomCancelLbl.Font.Size := 9;
  CustomCancelLbl.Font.Color := $00CCCCCC;
  CustomCancelLbl.Transparent := True;
  CustomCancelLbl.Cursor := crHandPoint;
  CustomCancelLbl.OnClick := @CustomCancelBtnClick;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpWelcome then
  begin
    DarkBottomBar.Visible := False;
    CustomNextBtn.Parent := WizardForm.WelcomePage;
    CustomNextBtn.Visible := True;
    CustomNextBtn.Width := ScaleX(280);
    CustomNextBtn.Height := ScaleY(44);
    CustomNextBtn.Left := (WizardForm.WelcomePage.Width - CustomNextBtn.Width) div 2;
    CustomNextBtn.Top := WizardForm.WelcomePage.Height - ScaleY(66);
    SetWindowRgn(CustomNextBtn.Handle, CreateRoundRectRgn(0, 0, CustomNextBtn.Width, CustomNextBtn.Height, CustomNextBtn.Height, CustomNextBtn.Height), True);
    CustomNextLbl.Caption := '立即安装';

    if PathEdit.Text <> '' then
      WizardForm.DirEdit.Text := PathEdit.Text;
    PathEdit.SelLength := 0;
    PathEdit.SelStart := Length(PathEdit.Text);
  end
  else if CurPageID = wpInstalling then
  begin
    // 安装进度条页面全暗黑化，根除任何白屏
    WizardForm.Color := $161312;
    WizardForm.InstallingPage.Color := $161312;
    WizardForm.MainPanel.Color := $161312;
    WizardForm.PageNameLabel.Font.Color := $00FFFFFF;
    WizardForm.PageDescriptionLabel.Font.Color := $00AAAAAA;
    WizardForm.StatusLabel.Font.Color := $00FFFFFF;
    WizardForm.FileNameLabel.Font.Color := $00888888;

    DarkBottomBar.Visible := True;
    CustomNextBtn.Visible := False;
    CustomCancelBtn.Parent := DarkBottomBar;
    CustomCancelBtn.Visible := True;
  end
  else if CurPageID = wpFinished then
  begin
    // 完成页面全暗黑化，隐藏多余侧边栏图，纯白高亮文字
    WizardForm.Color := $161312;
    WizardForm.FinishedPage.Color := $161312;
    WizardForm.WizardBitmapImage2.Visible := False;

    WizardForm.FinishedHeadingLabel.Font.Name := 'Segoe UI';
    WizardForm.FinishedHeadingLabel.Font.Size := 16;
    WizardForm.FinishedHeadingLabel.Font.Style := [fsBold];
    WizardForm.FinishedHeadingLabel.Font.Color := $0099D334;
    WizardForm.FinishedHeadingLabel.Left := ScaleX(48);
    WizardForm.FinishedHeadingLabel.Top := ScaleY(48);

    WizardForm.FinishedLabel.Font.Name := 'Segoe UI';
    WizardForm.FinishedLabel.Font.Size := 10;
    WizardForm.FinishedLabel.Font.Color := $00E0E0E0;
    WizardForm.FinishedLabel.Left := ScaleX(48);
    WizardForm.FinishedLabel.Top := ScaleY(100);
    // 明写「还要下载模型」：安装 ≠ 可用。不说这一句，用户点开就转写失败，
    // 而错误文案只会说「SenseVoice 组件未就齐」，完全指向不了「装完还要下模型」。
    WizardForm.FinishedLabel.Caption := '安装完成。首次使用需联网下载语音模型（约 290MB，国内镜像无需梯子）。' + #13#10 +
      '所有处理都在本机完成，音视频与字幕不会上传。';

    // 彻底解决 RunList 黑色文字在黑底看不见的问题 (纯白文字 + 彻底无边框)
    WizardForm.RunList.Color := $161312;
    WizardForm.RunList.Font.Name := 'Segoe UI';
    WizardForm.RunList.Font.Size := 10;
    WizardForm.RunList.Font.Color := $00FFFFFF;
    WizardForm.RunList.BorderStyle := bsNone;
    WizardForm.RunList.Left := ScaleX(48);
    WizardForm.RunList.Top := ScaleY(180);
    WizardForm.RunList.Width := WizardForm.FinishedPage.Width - ScaleX(96);

    // 将「立即体验」按钮直接放在 FinishedPage 内部，100% 绝对可见
    DarkBottomBar.Visible := False;
    CustomNextBtn.Parent := WizardForm.FinishedPage;
    CustomNextBtn.Visible := True;
    CustomNextBtn.Width := ScaleX(140);
    CustomNextBtn.Height := ScaleY(42);
    CustomNextBtn.Left := WizardForm.FinishedPage.Width - CustomNextBtn.Width - ScaleX(48);
    CustomNextBtn.Top := WizardForm.FinishedPage.Height - ScaleY(68);
    SetWindowRgn(CustomNextBtn.Handle, CreateRoundRectRgn(0, 0, CustomNextBtn.Width, CustomNextBtn.Height, CustomNextBtn.Height, CustomNextBtn.Height), True);
    CustomNextBtn.BringToFront;
    CustomNextLbl.Caption := '立即体验';
    CustomNextLbl.Font.Size := 10;
    CustomCancelBtn.Visible := False;
  end;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  // 路径与快捷方式已在欢迎页一站式搞定，必须彻底跳过目录选择、程序组、附加任务与准备页，点击立即安装直达解压进度！
  if (PageID = wpSelectDir) or (PageID = wpSelectProgramGroup) or (PageID = wpSelectTasks) or (PageID = wpReady) then
    Result := True
  else
    Result := False;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = wpWelcome then
  begin
    if Trim(PathEdit.Text) = '' then
    begin
      MsgBox('请指定有效的安装路径！', mbError, MB_OK);
      Result := False;
      Exit;
    end;
    WizardForm.DirEdit.Text := PathEdit.Text;
    if not DesktopCheckBox.Checked then
      WizardSelectTasks('!desktopicon')
    else
      WizardSelectTasks('desktopicon');
  end;
end;

{ ===== 卸载：清理运行期产物 ===== }
// 运行期写入的东西（模型、数据库、日志、缓存、配置覆盖、记忆库）都不在安装清单里，
// 不主动删就会永远留在安装目录。模型/历史数据对用户有价值，所以询问一次；
// 纯缓存与日志无条件清理，保证「卸不干净」不再出现。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  AppDir: String;
begin
  AppDir := ExpandConstant('{app}');

  if CurUninstallStep = usUninstall then
  begin
    if MsgBox('是否同时删除已下载的模型、历史数据库与本地配置？' + #13#10 + #13#10 +
              '选择「否」会保留它们（以后重新安装仍可继续使用），' + #13#10 +
              '但日志、缓存与更新包一定会被清理。',
              mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    begin
      DelTree(AppDir + '\models', True, True, True);
      DelTree(AppDir + '\tools', True, True, True);
      DelTree(AppDir + '\backups', True, True, True);
      DeleteFile(AppDir + '\voice2word.db');
      DeleteFile(AppDir + '\voice2word.db-wal');
      DeleteFile(AppDir + '\voice2word.db-shm');
      DeleteFile(AppDir + '\config.local.toml');
      DeleteFile(AppDir + '\typo_memory.json');
      DeleteFile(AppDir + '\glossary.txt');
      DeleteFile(AppDir + '\batch_queue.json');
      DeleteFile(AppDir + '\replace_rule_sets.json');
    end;
  end;

  if CurUninstallStep = usPostUninstall then
  begin
    // 目录此时只剩「用户选择保留」的数据（或已空）；空就删掉，非空保留。
    RemoveDir(AppDir);
  end;
end;

{ ===== 安装后自检：把「缺什么」当场说清楚 ===== }
// 逐项检查关键运行时组件是否存在且**完整**（入口 exe + 同目录 DLL）。
// 只报缺项，不阻止安装：缺 Whisper 仍可用 SenseVoice，缺 llama 只影响离线翻译。
function CheckRuntimeComponents(AppDir: String): String;
var
  Missing: String;
  DllDir: String;
  HasLlamaDll, HasWhisperDll: Boolean;
begin
  Missing := '';

  // 1) FFmpeg：必需。缺了连音频都抽不出来。
  if not FileExists(AppDir + '\tools\ffmpeg.exe') then
    Missing := Missing + #13#10 + '  - FFmpeg（tools\ffmpeg.exe）：必需，音频提取与转码都依赖它';

  // 1.5) 主程序自己的 VC++ 运行库必须在 exe 同目录。
  //      voice2word.exe 的导入表里直接写着 vcruntime140.dll；干净 Windows 上没有它。
  if not FileExists(AppDir + '\vcruntime140.dll') then
    Missing := Missing + #13#10 +
      '  - 主程序运行库 vcruntime140.dll（应在安装目录根部）：缺失时程序无法启动（0xC0000135）';

  // 2) llama.cpp：入口 exe 只有几 KB，真正代码在同目录 DLL。
  //    **只查 exe 会漏判**——这正是「本机好使、别人电脑不好使」的成因。
  DllDir := AppDir + '\tools';
  HasLlamaDll :=
    FileExists(DllDir + '\llama.dll') and
    FileExists(DllDir + '\llama-common.dll') and
    FileExists(DllDir + '\ggml.dll');
  if FileExists(DllDir + '\llama-completion.exe') and not HasLlamaDll then
    Missing := Missing + #13#10 +
      '  - llama.cpp 的同目录 DLL：离线翻译/润色会静默失败（缺 llama.dll / llama-common.dll / ggml.dll）';

  // 3) whisper.cpp：同样要连 DLL 一起查。
  DllDir := AppDir + '\tools\whisper-vulkan\whisper-1.8.4-windows-x64';
  HasWhisperDll := FileExists(DllDir + '\whisper.dll') and FileExists(DllDir + '\ggml.dll');
  if FileExists(DllDir + '\whisper-cli.exe') and not HasWhisperDll then
    Missing := Missing + #13#10 +
      '  - whisper.cpp 的同目录 DLL：Whisper 引擎会静默失败（缺 whisper.dll / ggml.dll）';

  // 4) 便携 Python：SenseVoice 与 CT-Punc 的运行环境。
  if not FileExists(AppDir + '\tools\python\python.exe') then
    Missing := Missing + #13#10 +
      '  - 内置便携 Python（tools\python\）：SenseVoice 与标点需要它（可在设置里指定系统 Python）';

  // 5) ffplay + 它的 av*/sw* DLL：预览的声音。
  //    官方的 ffplay.exe 是**共享库构建**，缺 DLL 就以 0xC0000135 秒退；
  //    开发机上有这些 DLL 在 PATH 上所以「本机好使」，客户机上则完全无声。
  if FileExists(AppDir + '\tools\ffplay.exe') then
  begin
    DllDir := AppDir + '\tools';
    if not FileExists(DllDir + '\avcodec-62.dll') and
       not FileExists(DllDir + '\avcodec-61.dll') and
       not FileExists(DllDir + '\avcodec-60.dll') then
      Missing := Missing + #13#10 +
        '  - ffplay 的同目录 DLL（av*.dll / sw*.dll）：视频预览将没有声音';
  end;

  // 6) 模型目录为空：首次使用必须联网下载（约 290MB 起）。
  //    这不是「缺件」，但用户必须知道——否则以为装完就能用。
  if not DirExists(AppDir + '\models\sensevoice') and
     not DirExists(AppDir + '\models\whisper') then
    Missing := Missing + #13#10 +
      '  - 语音模型（models\）：首次使用需联网下载约 290MB（国内镜像，无需梯子）';

  Result := Missing;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Missing: String;
begin
  if CurStep = ssPostInstall then
  begin
    Missing := CheckRuntimeComponents(ExpandConstant('{app}'));
    if Missing <> '' then
    begin
      MsgBox('安装完成，但发现以下组件缺失（相关功能会无法使用）：' + #13#10 + Missing + #13#10 + #13#10 +
             '可以在「性能设置 → 模型与组件」里一键补齐；缺 FFmpeg 时请重新安装本程序。',
             mbInformation, MB_OK);
    end;
  end;
end;

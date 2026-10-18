; =============================================================================
; Voice2Word — Inno Setup 极简现代安装向导脚本
; =============================================================================

#define MyAppName "Voice2Word"
#ifndef MyAppVersion
#define MyAppVersion "0.2.20"
#endif
#define MyAppPublisher "Voice2Word"
#define MyAppURL "https://github.com/ygs0422/Voice2Word"
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
ShowLanguageDialog=no
DisableWelcomePage=no
DisableReadyPage=yes
DisableDirPage=no
DisableProgramGroupPage=yes
DisableFinishedPage=no
UninstallDisplayIcon={app}\app.ico
UninstallDisplayName={#MyAppName}

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
Source: "..\config.toml"; DestDir: "{app}"; Flags: onlyifdoesntexist skipifsourcedoesntexist
Source: "..\tools\punc_runner.py"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\sensevoice_runner.py"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\ffmpeg.exe"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\tools\ffplay.exe"; DestDir: "{app}\tools"; Flags: ignoreversion skipifsourcedoesntexist
; 内置便携 Python：SenseVoice / CT-Punc 的运行环境，避免客户机「系统 Python 残缺」。
; 由 scripts/build_python_runtime.ps1 现场生成（tools/ 不进版本库），缺失时跳过。
Source: "..\tools\python\*"; DestDir: "{app}\tools\python"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"; WorkingDir: "{app}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon; IconFilename: "{app}\app.ico"

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent runasoriginaluser

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

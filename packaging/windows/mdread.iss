; Inno Setup script for mdread: a per-user (no admin) x64 installer.
;
; Build the release binary first, then compile from the repo root:
;   cargo build --release
;   ISCC packaging\windows\mdread.iss
; Output: target\installer\mdread-setup-<version>.exe
;
; Optional defines (ISCC /D<name>=<value>):
;   AppVersion  installer version; defaults to [package] version in Cargo.toml
;   SetupIcon   icon for the setup exe; defaults to assets\icons\mdread.ico,
;               pass /DSetupIcon= (empty) to build without one
;
; Compiles with Inno Setup 6.3+ and 7. The registry keys written here are
; mirrored by install.ps1 (build-from-source install); keep the two in sync.

#ifndef AppVersion
  ; First line starting with `version = "` is the [package] version: the
  ; dependency tables write theirs inline (`foo = { version = ... }`).
  #define CargoToml FileOpen(AddBackslash(SourcePath) + "..\..\Cargo.toml")
  #define VersionLine ""
  #sub ReadVersionLine
    #define Line FileRead(CargoToml)
    #if Pos('version = "', Line) == 1
      #define public VersionLine Line
    #endif
  #endsub
  #for {0; VersionLine == "" && !FileEof(CargoToml); 0} ReadVersionLine
  #expr FileClose(CargoToml)
  #if VersionLine == ""
    #error Could not read the version from Cargo.toml; pass /DAppVersion=x.y.z
  #endif
  #define AppVersion Copy(VersionLine, 12, RPos('"', VersionLine) - 12)
#endif
; VersionInfoVersion only accepts digits and dots: drop any "-beta.1" suffix.
#if Pos("-", AppVersion) > 0
  #define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion) - 1)
#else
  #define NumericVersion AppVersion
#endif
#ifndef SetupIcon
  #define SetupIcon "assets\icons\mdread.ico"
#endif

#define AppName "mdread"
#define AppExe "mdread.exe"
#define ProgId "mdread.Markdown"

[Setup]
; Never change AppId: upgrades and the uninstaller entry are keyed on it.
AppId={{697C8DA3-403A-4009-84FF-8E2DD5F4CEF1}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisherURL=https://github.com/vaughnd/mdread
AppSupportURL=https://github.com/vaughnd/mdread/issues
AppCopyright=Copyright (c) 2026 Vaughn Dickson and mdread contributors
VersionInfoVersion={#NumericVersion}
VersionInfoDescription={#AppName} setup
; Paths below are relative to the repo root.
SourceDir=..\..
OutputDir=target\installer
OutputBaseFilename=mdread-setup-{#AppVersion}
#if SetupIcon != ""
SetupIconFile={#SetupIcon}
#endif
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
; Per-user install into %LOCALAPPDATA%\Programs\mdread; never asks for admin.
PrivilegesRequired=lowest
DefaultDirName={userpf}\{#AppName}
; Fixed folder: install.ps1 detects a setup install by its uninstaller there.
DisableDirPage=yes
DisableProgramGroupPage=yes
MinVersion=10.0
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#if Ver >= EncodeVer(7, 0, 0)
SetupArchitecture=x64
#endif
; Follows the Windows light/dark setting from 6.6 on.
#if Ver >= EncodeVer(6, 6, 0)
WizardStyle=modern dynamic
#else
WizardStyle=modern
#endif
Compression=lzma2/max
SolidCompression=yes
; Explorer refreshes file-type icons and "Open with" after (un)install, and
; running programs see the PATH change.
ChangesAssociations=yes
ChangesEnvironment=yes
; A viewer is often left open during an upgrade: close it via Restart Manager,
; also in silent mode (winget), instead of failing to replace the exe.
CloseApplications=force

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add mdread to PATH (run ""mdread file.md"" from a terminal)"; GroupDescription: "Other:"

[Files]
Source: "target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Markdown viewer"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Markdown viewer"; Tasks: desktopicon

[Registry]
; App Paths: lets Win+R / `start mdread` find the exe without PATH.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\{#AppExe}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExe}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\{#AppExe}"; ValueType: string; ValueName: "Path"; ValueData: "{app}"

; ProgID that the extensions below point at.
Root: HKCU; Subkey: "Software\Classes\{#ProgId}"; ValueType: string; ValueName: ""; ValueData: "Markdown Document"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#ProgId}"; ValueType: string; ValueName: "FriendlyTypeName"; ValueData: "Markdown Document"
Root: HKCU; Subkey: "Software\Classes\{#ProgId}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"",0"
Root: HKCU; Subkey: "Software\Classes\{#ProgId}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""

; Per extension: the key itself (removed on uninstall only if left empty),
; mdread in "Open with", and the extension's default handler only when no
; other app has claimed it (Windows 11 lets the user pick in Settings).
; Uninstall walks entries in reverse, so OpenWithProgids goes before its parent.
#define ExtEntries(str Ext) \
  'Root: HKCU; Subkey: "Software\Classes\' + Ext + '"; Flags: uninsdeletekeyifempty' + NewLine + \
  'Root: HKCU; Subkey: "Software\Classes\' + Ext + '"; ValueType: string; ValueName: ""; ValueData: "' + ProgId + '"; Check: ExtDefaultIsFree(''' + Ext + ''')' + NewLine + \
  'Root: HKCU; Subkey: "Software\Classes\' + Ext + '\OpenWithProgids"; ValueType: string; ValueName: "' + ProgId + '"; ValueData: ""; Flags: uninsdeletevalue uninsdeletekeyifempty' + NewLine + \
  'Root: HKCU; Subkey: "Software\Classes\Applications\' + AppExe + '\SupportedTypes"; ValueType: string; ValueName: "' + Ext + '"; ValueData: ""' + NewLine + \
  'Root: HKCU; Subkey: "Software\' + AppName + '\Capabilities\FileAssociations"; ValueType: string; ValueName: "' + Ext + '"; ValueData: "' + ProgId + '"' + NewLine
#emit ExtEntries(".md")
#emit ExtEntries(".markdown")
#emit ExtEntries(".mdown")
#emit ExtEntries(".mkd")
#emit ExtEntries(".mdx")

; Applications\mdread.exe: name and icon in "Open with", limited to Markdown.
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#AppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"",0"
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""

; Capabilities + RegisteredApplications: lists mdread in Settings > Default apps.
Root: HKCU; Subkey: "Software\{#AppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\{#AppName}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#AppName}"
Root: HKCU; Subkey: "Software\{#AppName}\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Fast GPU-rendered Markdown viewer"
Root: HKCU; Subkey: "Software\{#AppName}\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: """{app}\{#AppExe}"",0"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#AppName}"; ValueData: "Software\{#AppName}\Capabilities"; Flags: uninsdeletevalue

[Run]
; Windows 11 only lets the user choose the default app: open mdread's page in
; Settings > Default apps (older builds ignore the query and show the list).
; Skipped when mdread already is the default, e.g. on upgrades.
Filename: "ms-settings:defaultapps?registeredAppUser={#AppName}"; Description: "Choose mdread as the default app for Markdown files"; Flags: shellexec postinstall nowait skipifsilent; Check: not MdreadIsDefault
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: postinstall nowait skipifsilent unchecked

; Settings in %APPDATA%\mdread are kept on uninstall.

[Code]
const
  ProgId = '{#ProgId}';
  EnvKey = 'Environment';

{ True when RootKey\Software\Classes\Ext names no default app, or names mdread. }
function ExtDefaultIsFreeIn(RootKey: Integer; Ext: String): Boolean;
var
  Current: String;
begin
  Result := (not RegQueryStringValue(RootKey, 'Software\Classes\' + Ext, '', Current)) or (Current = '') or (CompareText(Current, ProgId) = 0);
end;

{ True when no app, per-user or machine-wide, has claimed Ext yet, or it is
  already ours. Reads both hives instead of HKCR: once HKCU\Software\Classes\Ext
  exists (the entry before the check creates it), HKCR no longer shows the
  HKLM key's values. }
function ExtDefaultIsFree(Ext: String): Boolean;
begin
  Result := ExtDefaultIsFreeIn(HKCU, Ext) and ExtDefaultIsFreeIn(HKLM, Ext);
end;

{ True when .md already opens with mdread: the user's choice in Settings
  (UserChoiceLatest on current Windows 11 builds, else UserChoice), or, when
  the user never chose, the HKCR default. }
function MdreadIsDefault: Boolean;
var
  Key, Current: String;
begin
  Key := 'Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.md\';
  if RegQueryStringValue(HKCU, Key + 'UserChoiceLatest\ProgId', 'ProgId', Current) or
     RegQueryStringValue(HKCU, Key + 'UserChoice', 'ProgId', Current) then
    Result := CompareText(Current, ProgId) = 0
  else
    Result := RegQueryStringValue(HKCR, '.md', '', Current) and (CompareText(Current, ProgId) = 0);
end;

function NormalizeDir(Dir: String): String;
begin
  Result := Lowercase(RemoveBackslashUnlessRoot(Trim(Dir)));
end;

{ Removes Dir from the user PATH, or appends it when Add is set and it is not
  there yet, so repeated installs never duplicate the entry. The rest of the
  PATH is kept character for character, empty entries and a trailing ';'
  included: other tools append 'dir;' on the assumption that the PATH ends in
  ';' (the stock user PATH does), and uninstall gives back exactly what install
  found. Keeps the value REG_EXPAND_SZ, which Windows uses for the user PATH.
  install.ps1's Update-UserPath does the same. }
procedure UpdateUserPath(Dir: String; Add: Boolean);
var
  OldPath, NewPath, Rest, Item: String;
  P: Integer;
  Changed, First, Last: Boolean;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', OldPath) then
    OldPath := '';
  NewPath := '';
  Changed := False;
  First := True;
  Rest := OldPath;
  { repeat, not while: the piece after the last ';' counts too, even when empty }
  repeat
    P := Pos(';', Rest);
    Last := P = 0;
    if Last then
      Item := Rest
    else
    begin
      Item := Copy(Rest, 1, P - 1);
      Rest := Copy(Rest, P + 1, Length(Rest));
    end;
    if (Trim(Item) <> '') and (NormalizeDir(Item) = NormalizeDir(Dir)) then
    begin
      if Add then
        Exit; { already present: leave PATH untouched }
      Changed := True;
    end else
    begin
      if not First then
        NewPath := NewPath + ';';
      NewPath := NewPath + Item;
      First := False;
    end;
  until Last;
  if Add then
  begin
    { Here NewPath = OldPath. Dir goes before a trailing ';', so the PATH
      keeps ending in one and uninstall restores it exactly. }
    if NewPath = '' then
      NewPath := Dir
    else if NewPath[Length(NewPath)] = ';' then
      NewPath := NewPath + Dir + ';'
    else
      NewPath := NewPath + ';' + Dir;
    Changed := True;
  end;
  if Changed then
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', NewPath);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    UpdateUserPath(ExpandConstant('{app}'), WizardIsTaskSelected('addtopath'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Exts: array of String;
  Current: String;
  I: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    UpdateUserPath(ExpandConstant('{app}'), False);
    { Only clear an extension's default if it still points at mdread: another
      app may have taken it over since install. }
    Exts := ['.md', '.markdown', '.mdown', '.mkd', '.mdx'];
    for I := 0 to GetArrayLength(Exts) - 1 do
      if RegQueryStringValue(HKCU, 'Software\Classes\' + Exts[I], '', Current) and (CompareText(Current, ProgId) = 0) then
        RegDeleteValue(HKCU, 'Software\Classes\' + Exts[I], '');
  end;
end;

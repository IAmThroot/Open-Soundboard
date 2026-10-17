; Open Soundboard — Inno Setup 6 installer script.
;
; Built by GitHub Actions (see .github/workflows/release.yml), but it also
; works locally on Windows:
;   cargo build --release
;   iscc /DMyAppVersion=0.1.0 packaging\installer.iss
;
; Design decisions:
; - Per-user install (PrivilegesRequired=lowest): no UAC prompt, installs into
;   the user's own %LOCALAPPDATA%\Programs. Uninstalling never needs admin.
; - No "start with Windows" task: the user never asked for it, and a soundboard
;   that silently starts on login is a surprise. The in-app Settings dialog
;   offers tray behavior instead.
; - The app's own data (sounds, recordings, config) lives under %APPDATA%, NOT
;   under the install folder, so uninstalling leaves the user's soundboard
;   intact for a later reinstall.

#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif

#define AppName "Open Soundboard"
#define AppExeName "open-soundboard.exe"
#ifndef BuildDir
  #define BuildDir "..\target\release"
#endif

[Setup]
AppId={{E0C08056-6DE8-43EA-8DE5-C75C35E8C07C}
AppName={#AppName}
AppVersion={#MyAppVersion}
AppPublisher=Open Soundboard
DefaultDirName={localappdata}\Programs\Open Soundboard
DefaultGroupName={#AppName}
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=OpenSoundboard-Setup-{#MyAppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64
DisableProgramGroupPage=auto
; Long paths are not needed; keep the installer quiet about them.
UsedUserAreasWarning=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BuildDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion isreadme
; PORTABLE.txt is deliberately NOT shipped here: it explains the portable
; zip, which this installer does not produce. It travels with that build.

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Only the install folder; %APPDATA%\OpenSoundboard (sounds, recordings,
; config) is deliberately left alone so reinstalling restores everything.
Type: filesandordirs; Name: "{app}"
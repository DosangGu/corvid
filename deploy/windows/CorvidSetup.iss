#ifndef Payload
  #error Payload must point to the verified Desktop files
#endif
#ifndef Release
  #error Release must be the Desktop package version
#endif

[Setup]
AppId={{6F92E7F9-5DFB-41BF-8CCD-2B746106A7D9}
AppName=Corvid Desktop
AppVersion={#Release}
AppPublisher=Corvid
DefaultDirName={localappdata}\Programs\Corvid
DefaultGroupName=Corvid
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=no
RestartApplications=no
SetupIconFile={#Icon}
UninstallDisplayIcon={app}\corvid.ico
Compression=lzma2
SolidCompression=yes
OutputBaseFilename=corvid-desktop-{#Release}-x86_64-pc-windows-msvc-setup

[Files]
Source: "{#Payload}\*"; DestDir: "{app}\versions\{#Release}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#Payload}\corvid-launch.exe"; DestDir: "{app}\bin"; DestName: "corvid.exe"; Flags: onlyifdoesntexist
Source: "{#Payload}\corvid-desktop-launch.exe"; DestDir: "{app}\bin"; DestName: "corvid-desktop.exe"; Flags: onlyifdoesntexist
Source: "{#Payload}\corvid-agent-deck-launch.exe"; DestDir: "{app}\bin"; DestName: "corvid-agent-deck.exe"; Flags: onlyifdoesntexist
Source: "{#Icon}"; DestDir: "{app}"; DestName: "corvid.ico"; Flags: ignoreversion

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Icons]
Name: "{group}\Corvid Desktop"; Filename: "{app}\bin\corvid-desktop.exe"; IconFilename: "{app}\corvid.ico"; AppUserModelID: "Corvid.Desktop"
Name: "{group}\Corvid Terminal"; Filename: "{app}\bin\corvid.exe"; IconFilename: "{app}\corvid.ico"
Name: "{autodesktop}\Corvid Desktop"; Filename: "{app}\bin\corvid-desktop.exe"; IconFilename: "{app}\corvid.ico"; AppUserModelID: "Corvid.Desktop"; Tasks: desktopicon

[UninstallDelete]
Type: files; Name: "{app}\current.txt"
Type: files; Name: "{app}\current.txt.pending"

[Code]
function MoveFileEx(ExistingFile, NewFile: string; Flags: Cardinal): Boolean;
  external 'MoveFileExW@kernel32.dll stdcall';

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\install.json')) then
    Result := 'This folder belongs to the Corvid script installer. Select a different folder.';
end;

procedure CurStepChanged(Step: TSetupStep);
var
  PointerFile, TemporaryFile: String;
begin
  if Step <> ssPostInstall then
    exit;
  PointerFile := ExpandConstant('{app}\current.txt');
  TemporaryFile := PointerFile + '.pending';
  if not SaveStringToFile(TemporaryFile, '{#Release}' + #13#10, False) then
    RaiseException('Could not write the Corvid version pointer.');
  if not MoveFileEx(TemporaryFile, PointerFile, 9) then
    RaiseException('Could not activate this Corvid version.');
end;

procedure RemoveOwnedLinks(const Folder: String);
var
  Search: TFindRec;
  Launcher, FileName, Target: String;
  Shell, Shortcut: Variant;
begin
  Launcher := Lowercase(ExpandConstant('{app}\bin\corvid-agent-deck.exe'));
  if FindFirst(Folder + '\Corvid Agent Deck *.lnk', Search) then
  try
    Shell := CreateOleObject('WScript.Shell');
    repeat
      FileName := Folder + '\' + Search.Name;
      try
        Shortcut := Shell.CreateShortcut(FileName);
        Target := Shortcut.TargetPath;
        if Lowercase(Target) = Launcher then
          DeleteFile(FileName);
      except
        Log('An Agent Deck shortcut could not be inspected.');
      end;
    until not FindNext(Search);
  finally
    FindClose(Search);
  end;
end;

procedure CurUninstallStepChanged(Step: TUninstallStep);
begin
  if Step = usUninstall then
  begin
    RemoveOwnedLinks(ExpandConstant('{userprograms}'));
    RemoveOwnedLinks(ExpandConstant('{userstartup}'));
  end;
end;

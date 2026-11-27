#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif
#ifndef StageDir
  #define StageDir "..\dist\postbox-windows-x64"
#endif

[Setup]
AppId={{9F6C2B54-4B18-4D93-9E2B-7E7F8C3B2A10}
AppName=Postbox
AppVersion={#MyAppVersion}
DefaultDirName={localappdata}\Postbox
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=postbox-setup-x64
UninstallDisplayName=Postbox

[Files]
Source: "{#StageDir}\postbox.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\postboxctl.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\IBMPlex*.ttf"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\IBM-Plex-LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Postbox"; Filename: "{app}\postbox.exe"

[Run]
Filename: "{app}\postbox.exe"; Description: "Launch Postbox"; Flags: nowait postinstall skipifsilent

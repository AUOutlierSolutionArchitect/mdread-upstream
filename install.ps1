<#
.SYNOPSIS
Install mdread from source for the current user (Windows counterpart of install.sh).

.DESCRIPTION
Builds the release binary, copies it to %LOCALAPPDATA%\Programs\mdread, adds a
Start menu shortcut, puts the folder on the user PATH and registers mdread for
Markdown files under HKCU (no admin needed). Windows does not let programs make
themselves the default app, so Settings > Default apps is opened at the end
(unless mdread already is the default for .md).

Settings (%APPDATA%\mdread) are never touched, also not by -Uninstall.

The registry keys written here mirror packaging\windows\mdread.iss (the Inno
Setup installer), which installs to the same folder; keep the two in sync.

.PARAMETER NoBuild
Install the existing target\release\mdread.exe instead of running cargo.

.PARAMETER NoAssociate
Skip the file association and Default apps registration.

.PARAMETER NoDefaultApps
Do not open Settings > Default apps at the end.

.PARAMETER Uninstall
Remove everything this script adds.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\install.ps1
#>
[CmdletBinding()]
param(
    [switch]$NoBuild,
    [switch]$NoAssociate,
    [switch]$NoDefaultApps,
    [switch]$Uninstall
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# Write-Error would throw under 'Stop' with a noisy record; print and exit instead.
function Stop-Install([string]$Message) {
    [Console]::Error.WriteLine("install.ps1: $Message")
    exit 1
}

if ($env:OS -ne 'Windows_NT') {
    Stop-Install 'This script is for Windows; use ./install.sh on Linux and macOS.'
}

$AppName = 'mdread'
$ExeName = 'mdread.exe'
$ProgId = 'mdread.Markdown'
$Extensions = @('.md', '.markdown', '.mdown', '.mkd', '.mdx')
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\$AppName"
$Exe = Join-Path $InstallDir $ExeName
$Shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) "$AppName.lnk"
$CapabilitiesKey = "Software\$AppName\Capabilities"
$AppPathsKey = "Software\Microsoft\Windows\CurrentVersion\App Paths\$ExeName"

# Always the 64-bit view, like the x64 Inno installer, even from a 32-bit shell.
$Hkcu = [Microsoft.Win32.RegistryKey]::OpenBaseKey('CurrentUser', 'Registry64')
$Hklm = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', 'Registry64')
$Hkcr = [Microsoft.Win32.RegistryKey]::OpenBaseKey('ClassesRoot', 'Registry64')

if (-not ('MdreadInstall.Native' -as [type])) {
    Add-Type -Namespace MdreadInstall -Name Native -MemberDefinition @'
[DllImport("shell32.dll")]
public static extern void SHChangeNotify(int eventId, uint flags, IntPtr item1, IntPtr item2);

[DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, UIntPtr wParam, string lParam,
    uint flags, uint timeout, out UIntPtr result);
'@
}

# Refreshes Explorer's file-type icons and "Open with" lists.
function Send-AssocChanged {
    $SHCNE_ASSOCCHANGED = 0x08000000
    [MdreadInstall.Native]::SHChangeNotify($SHCNE_ASSOCCHANGED, 0, [IntPtr]::Zero, [IntPtr]::Zero)
}

# Tells Explorer (and terminals started from it) that the user PATH changed.
function Send-EnvironmentChanged {
    $HWND_BROADCAST = [IntPtr]0xffff
    $WM_SETTINGCHANGE = 0x1a
    $SMTO_ABORTIFHUNG = 0x2
    $result = [UIntPtr]::Zero
    [void][MdreadInstall.Native]::SendMessageTimeout($HWND_BROADCAST, $WM_SETTINGCHANGE, [UIntPtr]::Zero,
        'Environment', $SMTO_ABORTIFHUNG, 5000, [ref]$result)
}

function Set-RegValue([string]$Path, [string]$Name, $Value, [string]$Kind = 'String') {
    $key = $Hkcu.CreateSubKey($Path)
    try { $key.SetValue($Name, $Value, [Microsoft.Win32.RegistryValueKind]$Kind) } finally { $key.Close() }
}

function Remove-RegValue([string]$Path, [string]$Name) {
    $key = $Hkcu.OpenSubKey($Path, $true)
    if ($key) {
        try { $key.DeleteValue($Name, $false) } finally { $key.Close() }
    }
}

function Remove-RegKeyIfEmpty([string]$Path) {
    $key = $Hkcu.OpenSubKey($Path)
    if (-not $key) { return }
    $empty = ($key.SubKeyCount -eq 0) -and ($key.ValueCount -eq 0)
    $key.Close()
    if ($empty) { $Hkcu.DeleteSubKey($Path, $false) }
}

function Get-RegString($Root, [string]$Path, [string]$Name) {
    $key = $Root.OpenSubKey($Path)
    if (-not $key) { return '' }
    try { return [string]$key.GetValue($Name, '') } finally { $key.Close() }
}

# True when $Root\Software\Classes\$Ext names no default app, or names mdread.
function Test-ExtDefaultFree($Root, [string]$Ext) {
    $current = Get-RegString $Root "Software\Classes\$Ext" ''
    return ($current -eq '') -or ($current -ieq $ProgId)
}

# True when .md already opens with mdread: the user's choice in Settings
# (UserChoiceLatest on current Windows 11 builds, else UserChoice), or, when the
# user never chose, the HKCR default.
function Test-MdreadIsDefault {
    $fileExts = 'Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.md'
    $choice = Get-RegString $Hkcu "$fileExts\UserChoiceLatest\ProgId" 'ProgId'
    if ($choice -eq '') { $choice = Get-RegString $Hkcu "$fileExts\UserChoice" 'ProgId' }
    if ($choice -eq '') { $choice = Get-RegString $Hkcr '.md' '' }
    return $choice -ieq $ProgId
}

function Test-SameDir([string]$A, [string]$B) {
    $left = [Environment]::ExpandEnvironmentVariables($A.Trim()).TrimEnd('\')
    $right = [Environment]::ExpandEnvironmentVariables($B.Trim()).TrimEnd('\')
    return $left -ieq $right
}

# Removes $Dir from the user PATH, or appends it when $Add is set and it is not
# there yet, so repeated runs never duplicate it. The rest of the PATH is kept
# character for character, empty entries and a trailing ';' included: other
# tools append 'dir;' on the assumption that the PATH ends in ';' (the stock
# user PATH does), and -Uninstall gives back exactly what the install found.
# Read unexpanded and written back as REG_EXPAND_SZ so %VARS% in other entries
# survive (setx would expand them). mdread.iss's UpdateUserPath does the same.
function Update-UserPath([string]$Dir, [bool]$Add) {
    $key = $Hkcu.CreateSubKey('Environment')
    try {
        $old = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
        $parts = @($old -split ';')
        $kept = @($parts | Where-Object { $_.Trim() -eq '' -or -not (Test-SameDir $_ $Dir) })
        if ($Add) {
            if ($kept.Count -ne $parts.Count) { return $false }
            # $Dir goes before a trailing ';', so the PATH keeps ending in one
            # and -Uninstall restores it exactly.
            if ($old -eq '') { $new = $Dir }
            elseif ($old.EndsWith(';')) { $new = "$old$Dir;" }
            else { $new = "$old;$Dir" }
        } elseif ($kept.Count -eq $parts.Count) {
            return $false
        } else {
            $new = $kept -join ';'
        }
        $key.SetValue('Path', $new, [Microsoft.Win32.RegistryValueKind]::ExpandString)
        return $true
    } finally {
        $key.Close()
    }
}

function Register-Associations {
    $open = "`"$Exe`" `"%1`""
    $icon = "`"$Exe`",0"

    Set-RegValue "Software\Classes\$ProgId" '' 'Markdown Document'
    Set-RegValue "Software\Classes\$ProgId" 'FriendlyTypeName' 'Markdown Document'
    Set-RegValue "Software\Classes\$ProgId\DefaultIcon" '' $icon
    Set-RegValue "Software\Classes\$ProgId\shell\open\command" '' $open

    Set-RegValue "Software\Classes\Applications\$ExeName" 'FriendlyAppName' $AppName
    Set-RegValue "Software\Classes\Applications\$ExeName\DefaultIcon" '' $icon
    Set-RegValue "Software\Classes\Applications\$ExeName\shell\open\command" '' $open

    Set-RegValue $CapabilitiesKey 'ApplicationName' $AppName
    Set-RegValue $CapabilitiesKey 'ApplicationDescription' 'Fast GPU-rendered Markdown viewer'
    Set-RegValue $CapabilitiesKey 'ApplicationIcon' $icon

    foreach ($ext in $Extensions) {
        Set-RegValue "Software\Classes\$ext\OpenWithProgids" $ProgId ''
        Set-RegValue "Software\Classes\Applications\$ExeName\SupportedTypes" $ext ''
        Set-RegValue "$CapabilitiesKey\FileAssociations" $ext $ProgId
        # Claim the extension's default only if no app, per-user or machine-wide,
        # has it yet; Windows 11 lets the user choose. Reads both hives instead of
        # HKCR: once the HKCU key exists (OpenWithProgids above creates it), HKCR
        # no longer shows the HKLM key's values.
        if ((Test-ExtDefaultFree $Hkcu $ext) -and (Test-ExtDefaultFree $Hklm $ext)) {
            Set-RegValue "Software\Classes\$ext" '' $ProgId
        }
    }

    Set-RegValue 'Software\RegisteredApplications' $AppName $CapabilitiesKey
}

function Unregister-Associations {
    foreach ($ext in $Extensions) {
        Remove-RegValue "Software\Classes\$ext\OpenWithProgids" $ProgId
        Remove-RegKeyIfEmpty "Software\Classes\$ext\OpenWithProgids"
        # Leave the default alone if another app has taken it over since.
        if ((Get-RegString $Hkcu "Software\Classes\$ext" '') -ieq $ProgId) {
            Remove-RegValue "Software\Classes\$ext" ''
        }
        Remove-RegKeyIfEmpty "Software\Classes\$ext"
    }
    $Hkcu.DeleteSubKeyTree("Software\Classes\$ProgId", $false)
    $Hkcu.DeleteSubKeyTree("Software\Classes\Applications\$ExeName", $false)
    $Hkcu.DeleteSubKeyTree("Software\$AppName", $false)
    Remove-RegValue 'Software\RegisteredApplications' $AppName
}

$innoUninstaller = Join-Path $InstallDir 'unins000.exe'

if ($Uninstall) {
    if (Test-Path $innoUninstaller) {
        Stop-Install "mdread was installed with the setup program; uninstall it from Settings > Apps (or run $innoUninstaller)."
    }
    Unregister-Associations
    $Hkcu.DeleteSubKeyTree($AppPathsKey, $false)
    $pathChanged = Update-UserPath $InstallDir $false
    if (Test-Path $Shortcut) { Remove-Item $Shortcut -Force }
    if (Test-Path $Exe) {
        try {
            Remove-Item $Exe -Force
        } catch {
            Stop-Install "Could not remove $Exe; close mdread and run this again."
        }
    }
    if ((Test-Path $InstallDir) -and -not (Get-ChildItem -Force $InstallDir)) { Remove-Item $InstallDir -Force }
    Send-AssocChanged
    if ($pathChanged) { Send-EnvironmentChanged }
    Write-Host "Uninstalled mdread. Settings in $env:APPDATA\$AppName were kept."
    exit 0
}

$repo = $PSScriptRoot
$built = Join-Path $repo "target\release\$ExeName"

if (-not $NoBuild) {
    # Always build: cargo skips the work when nothing changed, and guarding on
    # the binary's existence would install a stale build after source edits.
    $cargo = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cargo) {
        $cargo = $cargo.Path
    } else {
        # rustup adds this to PATH, but shells started before the install miss it.
        $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
        if (-not (Test-Path $cargo)) {
            Stop-Install 'cargo not found. Install Rust from https://rustup.rs (MSVC toolchain) or pass -NoBuild.'
        }
    }
    Push-Location $repo
    # cargo reports progress on stderr, which 'Stop' turns into a terminating
    # error in some hosts; rely on the exit code instead.
    $ErrorActionPreference = 'Continue'
    try {
        & $cargo build --release
        if ($LASTEXITCODE -ne 0) {
            Stop-Install "cargo build failed (exit code $LASTEXITCODE)."
        }
    } finally {
        $ErrorActionPreference = 'Stop'
        Pop-Location
    }
}
if (-not (Test-Path $built)) {
    Stop-Install "$built not found; build it with 'cargo build --release' or drop -NoBuild."
}

if (Test-Path $innoUninstaller) {
    Write-Warning 'mdread was installed with the setup program; its files will be replaced in place.'
}
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
try {
    Copy-Item $built $Exe -Force
} catch {
    Stop-Install "Could not replace $Exe; close mdread and run this again."
}

$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut($Shortcut)
$lnk.TargetPath = $Exe
$lnk.Description = 'Markdown viewer'
$lnk.IconLocation = "$Exe,0"
$lnk.Save()
[void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($shell)

# App Paths: lets Win+R / `start mdread` find the exe without PATH.
Set-RegValue $AppPathsKey '' $Exe
Set-RegValue $AppPathsKey 'Path' $InstallDir

$pathChanged = Update-UserPath $InstallDir $true
if ($pathChanged) { Send-EnvironmentChanged }

Write-Host "Installed $Exe and the Start menu shortcut."
if ($pathChanged) {
    Write-Host "Added $InstallDir to your PATH; open a new terminal to run 'mdread'."
}

if (-not $NoAssociate) {
    Register-Associations
    Send-AssocChanged
    Write-Host 'Registered mdread for Markdown files (.md .markdown .mdown .mkd .mdx).'
    if (Test-MdreadIsDefault) {
        Write-Host 'mdread is already the default for .md.'
    } elseif (-not $NoDefaultApps) {
        # Windows only lets the user pick the default app: open mdread's page in
        # Settings > Default apps (older builds ignore the query and show the list).
        Write-Host 'Opening Settings > Default apps: choose mdread there to make it the default.'
        Start-Process "ms-settings:defaultapps?registeredAppUser=$AppName"
    }
}

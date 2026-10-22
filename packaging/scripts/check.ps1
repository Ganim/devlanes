<#
.SYNOPSIS
Dry run of the Windows packaging: install.ps1's helpers, package.ps1 and install.ps1
against a local release directory. CI runs it on Windows.

.DESCRIPTION
Without -BinDir the archives hold fake files. With -BinDir pointing at real
devlanes.exe, devlanesw.exe and devlanes-hosts-service.exe, the check also runs the
installed devlanes.exe and reinstalls while a daemon runs from it: the running
executable must be renamed to .old-*, the daemon must keep running, and doctor
must delete the old file once `daemon stop` ended it. That daemon uses its own
DEVLANES_HOME under the temporary directory, so it never meets the user's daemon.

install.ps1 runs in a child process of the same PowerShell (powershell.exe or pwsh)
with DEVLANES_NO_MODIFY_PATH=1, and the check fails if the user PATH changed.

.EXAMPLE
pwsh packaging/scripts/check.ps1 -BinDir target/release
#>
param([string]$BinDir)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0

$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$installScript = Join-Path $root 'install.ps1'
$packageScript = Join-Path $root 'packaging/scripts/package.ps1'
$shell = (Get-Process -Id $PID).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ("devlanes-check-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
$daemon = $null

function Assert-Equal {
    param($Actual, $Expected, [string]$What)
    if ($Actual -cne $Expected) {
        throw "check.ps1: ${What}: expected '$Expected', got '$Actual'"
    }
}

function Assert-True {
    param([bool]$Condition, [string]$What)
    if (-not $Condition) { throw "check.ps1: $What" }
}

function Invoke-Child {
    <# Runs PowerShell code in a child process of this PowerShell; returns exit code and output. #>
    param([string]$Command, [hashtable]$Environment)
    # Windows PowerShell turns a native command's stderr into a terminating error under Stop.
    $ErrorActionPreference = 'Continue'
    $saved = @{}
    foreach ($name in $Environment.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name, $Environment[$name])
    }
    try {
        $output = & $shell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command $Command 2>&1 | Out-String
        return @{ Code = $LASTEXITCODE; Output = $output }
    } finally {
        foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
    }
}

function Invoke-DevlanesExe {
    <# Runs the installed devlanes.exe; returns its exit code and output. #>
    param([string]$Exe, [string[]]$Arguments)
    $ErrorActionPreference = 'Continue'
    $output = & $Exe @Arguments 2>&1 | Out-String
    return @{ Code = $LASTEXITCODE; Output = $output }
}

function Get-UserPath {
    <# The raw user PATH, to prove the installs left it alone. #>
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        return [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } finally {
        $key.Dispose()
    }
}

# install.ps1 runs its installer when loaded, so only its function definitions are
# taken from the parse tree.
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($installScript, [ref]$tokens, [ref]$errors)
Assert-Equal $errors.Count 0 'install.ps1 parse errors'
$isFunction = { param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }
foreach ($definition in $ast.FindAll($isFunction, $false)) {
    . ([ScriptBlock]::Create($definition.Extent.Text))
}

Assert-Equal (Add-DevlanesPathEntry '' 'C:\acme') 'C:\acme' 'PATH from empty'
Assert-Equal (Add-DevlanesPathEntry '%USERPROFILE%\bin;;C:\tools' 'C:\acme') 'C:\acme;%USERPROFILE%\bin;;C:\tools' 'PATH keeps the rest byte for byte'
Assert-Equal (Add-DevlanesPathEntry 'C:\tools;c:\ACME\' 'C:\acme') $null 'PATH entry differing in case and trailing \'
Assert-Equal (Add-DevlanesPathEntry '"C:\acme";C:\tools' 'C:\acme') $null 'quoted PATH entry'
Assert-Equal (Add-DevlanesPathEntry '%LOCALAPPDATA%\Programs\devlanes' (Join-Path $env:LOCALAPPDATA 'Programs\devlanes')) $null 'PATH entry with a variable'
Assert-Equal (Get-DevlanesTarget 'AMD64') 'x86_64-pc-windows-msvc' 'x64 target'
Assert-Equal (Get-DevlanesTarget 'ARM64') 'aarch64-pc-windows-msvc' 'arm64 target'
$refused = $false
try { Get-DevlanesTarget 'x86' | Out-Null } catch { $refused = $true }
Assert-True $refused '32-bit Windows must be refused'
Assert-True ((Get-DevlanesArchitecture) -in @('AMD64', 'ARM64', 'x86')) 'architecture name'

$sums = "$('a' * 64)  devlanes-1.2.3-x86_64-unknown-linux-gnu.tar.gz`n$('B' * 64)  devlanes-1.2.3-x86_64-pc-windows-msvc.zip`n"
$found = Find-DevlanesArchive $sums 'x86_64-pc-windows-msvc' '' 'SHA256SUMS'
Assert-Equal $found.Version '1.2.3' 'version from SHA256SUMS'
Assert-Equal $found.Hash ('b' * 64) 'checksum from SHA256SUMS'
Assert-Equal $found.Archive 'devlanes-1.2.3-x86_64-pc-windows-msvc.zip' 'archive from SHA256SUMS'
foreach ($case in @(@('x86_64-pc-windows-msvc', '1.2.4'), @('aarch64-pc-windows-msvc', ''))) {
    $refused = $false
    try { Find-DevlanesArchive $sums $case[0] $case[1] 'SHA256SUMS' | Out-Null } catch { $refused = $true }
    Assert-True $refused "SHA256SUMS lookup of $($case -join ' ') must fail"
}

$pathBefore = Get-UserPath
New-Item -ItemType Directory -Path $work | Out-Null
try {
    if ($BinDir) {
        $BinDir = (Resolve-Path -LiteralPath $BinDir).Path
        $version = ((& (Join-Path $BinDir 'devlanes.exe') --version) -split ' ')[1]
    } else {
        $version = '0.0.0-check'
        $BinDir = Join-Path $work 'bin'
        New-Item -ItemType Directory -Path $BinDir | Out-Null
        foreach ($name in @('devlanes.exe', 'devlanesw.exe', 'devlanes-hosts-service.exe')) {
            Set-Content -LiteralPath (Join-Path $BinDir $name) -Value "fake $name $version"
        }
    }
    $target = Get-DevlanesTarget (Get-DevlanesArchitecture)
    $name = "devlanes-$version-$target"

    # The same inputs must give the same bytes.
    $hashes = foreach ($run in 1, 2) {
        $out = Join-Path $work "dist$run"
        & $shell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $packageScript -Version $version -Target $target -OutDir $out -BinDir $BinDir | Out-Null
        Assert-Equal $LASTEXITCODE 0 'package.ps1 exit code'
        (Get-FileHash -LiteralPath (Join-Path $out "$name.zip") -Algorithm SHA256).Hash.ToLowerInvariant()
    }
    Assert-Equal $hashes[1] $hashes[0] 'archive hash of a second run'

    # A local copy of the release layout install.ps1 downloads from.
    $release = Join-Path $work 'release'
    $versioned = Join-Path $release "download/v$version"
    $latest = Join-Path $release 'latest/download'
    New-Item -ItemType Directory -Force -Path $versioned, $latest | Out-Null
    Copy-Item -LiteralPath (Join-Path $work "dist1/$name.zip") -Destination $versioned
    $line = "$($hashes[0])  $name.zip`n"
    [IO.File]::WriteAllText((Join-Path $versioned 'SHA256SUMS'), $line)
    [IO.File]::WriteAllText((Join-Path $latest 'SHA256SUMS'), $line)

    $installed = Join-Path $work 'installed dir'
    $environment = @{
        DEVLANES_INSTALL_BASE_URL = ([Uri]$release).AbsoluteUri
        DEVLANES_INSTALL_DIR = $installed
        DEVLANES_NO_MODIFY_PATH = '1'
        DEVLANES_VERSION = $null
    }
    $run = Invoke-Child "& '$installScript'" $environment
    Assert-Equal $run.Code 0 "install.ps1 as a file: $($run.Output)"
    foreach ($file in @('LICENSE', 'devlanes.exe', 'devlanesw.exe', 'devlanes-hosts-service.exe')) {
        Assert-True (Test-Path -LiteralPath (Join-Path $installed $file)) "$file installed"
    }
    Assert-True ($run.Output -match [Regex]::Escape("Installed devlanes $version to $installed")) "install message: $($run.Output)"

    # `irm | iex` runs the text in the caller's session; a pinned version this time.
    $environment.DEVLANES_VERSION = "v$version"
    $run = Invoke-Child "Get-Content -Raw -LiteralPath '$installScript' | Invoke-Expression" $environment
    Assert-Equal $run.Code 0 "install.ps1 through Invoke-Expression: $($run.Output)"
    Assert-True (@(Get-ChildItem -LiteralPath $installed -Filter '*.old-*').Count -eq 0) 'unlocked old files are deleted'

    [IO.File]::WriteAllText((Join-Path $versioned 'SHA256SUMS'), "$('0' * 64)  $name.zip`n")
    $run = Invoke-Child "& '$installScript'" $environment
    Assert-True ($run.Code -ne 0 -and $run.Output -match 'checksum mismatch') "a wrong checksum must fail: $($run.Output)"
    [IO.File]::WriteAllText((Join-Path $versioned 'SHA256SUMS'), $line)

    if ($version -ne '0.0.0-check') {
        $exe = Join-Path $installed 'devlanes.exe'
        Assert-Equal (Invoke-DevlanesExe $exe @('--version')).Output.Trim() "devlanes $version" 'installed devlanes --version'

        # A daemon running from the installed file, with a home of its own.
        $env:DEVLANES_HOME = Join-Path $work 'h'
        $start = New-Object Diagnostics.ProcessStartInfo $exe
        $start.Arguments = 'daemon run'
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $daemon = [Diagnostics.Process]::Start($start)
        $deadline = (Get-Date).AddSeconds(60)
        do {
            Start-Sleep -Milliseconds 500
            $status = Invoke-DevlanesExe $exe @('daemon', 'status', '--json')
        } until ($status.Code -eq 0 -or $daemon.HasExited -or (Get-Date) -gt $deadline)
        Assert-Equal $status.Code 0 "daemon status before the reinstall: $($status.Output)"

        $run = Invoke-Child "& '$installScript'" $environment
        Assert-Equal $run.Code 0 "install.ps1 over a running daemon: $($run.Output)"
        $old = @(Get-ChildItem -LiteralPath $installed -Filter 'devlanes.exe.old-*')
        Assert-Equal $old.Count 1 'renamed running executable'
        Assert-True ($run.Output -match 'devlanes daemon restart') "restart advice: $($run.Output)"
        Assert-True (-not $daemon.HasExited) 'the daemon keeps running through the reinstall'
        $status = Invoke-DevlanesExe $exe @('daemon', 'status', '--json')
        Assert-Equal $status.Code 0 "daemon status after the reinstall: $($status.Output)"

        $stop = Invoke-DevlanesExe $exe @('daemon', 'stop', '--yes', '--quiet')
        Assert-Equal $stop.Code 0 "daemon stop: $($stop.Output)"
        Assert-True ($daemon.WaitForExit(30000)) 'the daemon exits after daemon stop'
        $doctor = (Invoke-DevlanesExe $exe @('doctor', '--output', 'plain')).Output
        Assert-True (-not (Test-Path -LiteralPath $old[0].FullName)) "doctor deletes the old executable: $doctor"
        Assert-True ($doctor -match 'deleted 1 old executable') "doctor reports the deletion: $doctor"
        Remove-Item Env:DEVLANES_HOME
    }
    Assert-Equal (Get-UserPath) $pathBefore 'user PATH'
} finally {
    if ($daemon -and -not $daemon.HasExited) { $daemon.Kill() }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Output "Windows packaging: dry run passed ($($PSVersionTable.PSEdition) $($PSVersionTable.PSVersion))"

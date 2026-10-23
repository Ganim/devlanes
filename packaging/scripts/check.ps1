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
DEVLANES_HOME (and ports) under the temporary directory, so it never meets the
user's daemon.

install.ps1 runs in a child process of the same PowerShell (powershell.exe or pwsh)
with DEVLANES_NO_MODIFY_PATH=1, and the check fails if the user PATH changed. Every
child process has a timeout, after which it is killed with its own children.

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

function ConvertTo-ProcessArgument {
    <# Quotes one argument for a Windows command line (CommandLineToArgvW rules). #>
    param([AllowEmptyString()][string]$Value)
    if ($Value -and $Value -notmatch '[\s"]') { return $Value }
    # Backslashes are doubled only before a quote, including the closing one.
    $escaped = ($Value -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1'
    return '"' + $escaped + '"'
}

function Invoke-Process {
    <#
    Runs a program and returns its exit code and output (stdout, then stderr). After
    TimeoutSeconds its process tree is killed and the call throws.
    #>
    param([string]$FilePath, [string[]]$Arguments, [int]$TimeoutSeconds)
    # Windows PowerShell turns a native command's stderr into a terminating error under Stop.
    $ErrorActionPreference = 'Continue'
    $start = New-Object Diagnostics.ProcessStartInfo $FilePath
    $start.Arguments = @($Arguments | ForEach-Object { ConvertTo-ProcessArgument $_ }) -join ' '
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($start)
    try {
        $process.StandardInput.Close()
        # Both pipes drain in the background, so a child that fills one never blocks.
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $exited = $process.WaitForExit($TimeoutSeconds * 1000)
        if (-not $exited) {
            # Windows PowerShell's Process.Kill has no tree option.
            & taskkill.exe /PID $process.Id /T /F 2>&1 | Out-Null
            $process.WaitForExit(10000) | Out-Null
        }
        # A grandchild that inherited the pipes (a daemon) can hold them open after exit.
        [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($stdout, $stderr), 10000) | Out-Null
        $output = (@($stdout, $stderr) | Where-Object { $_.IsCompleted } | ForEach-Object { $_.Result }) -join ''
        if (-not $exited) {
            throw "check.ps1: killed after $TimeoutSeconds s: $FilePath $($start.Arguments)`n$output"
        }
        return @{ Code = $process.ExitCode; Output = $output }
    } finally {
        $process.Dispose()
    }
}

function Invoke-Child {
    <# Runs PowerShell code in a child process of this PowerShell; returns exit code and output. #>
    param([string]$Command, [hashtable]$Environment, [int]$TimeoutSeconds = 300)
    $saved = @{}
    foreach ($name in $Environment.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name, $Environment[$name])
    }
    try {
        # Encoded, so the command needs no quoting for the command line. Text output,
        # else an encoded command writes its host output and errors as CLIXML.
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Command))
        $arguments = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-OutputFormat', 'Text', '-EncodedCommand', $encoded)
        return Invoke-Process $shell $arguments $TimeoutSeconds
    } finally {
        foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
    }
}

function Invoke-DevlanesExe {
    <# Runs the installed devlanes.exe; returns its exit code and output. #>
    param([string]$Exe, [string[]]$Arguments, [int]$TimeoutSeconds = 60)
    return Invoke-Process $Exe $Arguments $TimeoutSeconds
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
# taken from the parse tree (they sit inside the script block that holds its body).
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($installScript, [ref]$tokens, [ref]$errors)
Assert-Equal $errors.Count 0 'install.ps1 parse errors'
$isFunction = { param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }
foreach ($definition in $ast.FindAll($isFunction, $true)) {
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
    # A devlanes.exe ahead of the install directory is reported; one behind it is not.
    $shadowing = Join-Path $work 'shadowing'
    $installDir = Join-Path $work 'install-dir'
    New-Item -ItemType Directory -Path $shadowing, $installDir | Out-Null
    Set-Content -LiteralPath (Join-Path $shadowing 'devlanes.cmd') -Value '@exit /b 0'
    # The extension comes from PATHEXT, usually upper case.
    $shadow = Find-DevlanesShadow "C:\acme<>;$shadowing\;$installDir" $installDir
    Assert-True ($shadow -ieq "$shadowing\devlanes.cmd") "devlanes before the install directory: $shadow"
    Assert-Equal (Find-DevlanesShadow "$installDir;$shadowing" $installDir) $null 'devlanes after the install directory'

    if ($BinDir) {
        $BinDir = (Resolve-Path -LiteralPath $BinDir).Path
        $version = ((Invoke-DevlanesExe (Join-Path $BinDir 'devlanes.exe') @('--version')).Output.Trim() -split ' ')[1]
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
        $package = Invoke-Process $shell @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $packageScript,
            '-Version', $version, '-Target', $target, '-OutDir', $out, '-BinDir', $BinDir) 300
        Assert-Equal $package.Code 0 "package.ps1 exit code: $($package.Output)"
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
    $shipped = @('LICENSE', 'devlanes-hosts-service.exe', 'devlanes.exe', 'devlanesw.exe')
    $present = @(Get-ChildItem -LiteralPath $installed -Force | ForEach-Object { $_.Name } | Sort-Object)
    Assert-Equal ($present -join ' ') (($shipped | Sort-Object) -join ' ') 'installed files'
    Assert-True ($run.Output -match [Regex]::Escape("Installed devlanes $version to $installed")) "install message: $($run.Output)"

    # `irm | iex` runs the text in the caller's session, which must keep its own
    # variables and gain no functions or variables; a pinned version this time.
    $environment.DEVLANES_VERSION = "v$version"
    $before = '$Version = $InstallDir = $NoModifyPath = ''mine''
$names = @(Get-Variable | ForEach-Object Name) + @(Get-ChildItem Function: | ForEach-Object Name)'
    $after = '$left = @(Get-Variable | ForEach-Object Name) + @(Get-ChildItem Function: | ForEach-Object Name) | Where-Object { $names -notcontains $_ -and $_ -ne ''names'' }
if ($left) { throw "left in the session: $($left -join '', '')" }
if (@($Version, $InstallDir, $NoModifyPath) -ne ''mine'') { throw ''the session lost its own $Version, $InstallDir or $NoModifyPath'' }'
    $run = Invoke-Child "$before`nGet-Content -Raw -LiteralPath '$installScript' | Invoke-Expression`n$after" $environment
    Assert-Equal $run.Code 0 "install.ps1 through Invoke-Expression: $($run.Output)"
    Assert-True (@(Get-ChildItem -LiteralPath $installed -Filter '*.old-*').Count -eq 0) 'unlocked old files are deleted'

    [IO.File]::WriteAllText((Join-Path $versioned 'SHA256SUMS'), "$('0' * 64)  $name.zip`n")
    $run = Invoke-Child "& '$installScript'" $environment
    Assert-True ($run.Code -ne 0 -and $run.Output -match 'checksum mismatch') "a wrong checksum must fail: $($run.Output)"
    [IO.File]::WriteAllText((Join-Path $versioned 'SHA256SUMS'), $line)

    if ($version -ne '0.0.0-check') {
        $exe = Join-Path $installed 'devlanes.exe'
        Assert-Equal (Invoke-DevlanesExe $exe @('--version')).Output.Trim() "devlanes $version" 'installed devlanes --version'

        # A daemon running from the installed file, with a home and ports of its own.
        $env:DEVLANES_HOME = Join-Path $work 'h'
        New-Item -ItemType Directory -Path $env:DEVLANES_HOME | Out-Null
        [IO.File]::WriteAllText((Join-Path $env:DEVLANES_HOME 'config.yaml'),
            "ports:`n  range: 47100-47179`nproxy:`n  https_port: 47190`n  http_port: 47191`n")
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
    Remove-Item Env:DEVLANES_HOME -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Output "Windows packaging: dry run passed ($($PSVersionTable.PSEdition) $($PSVersionTable.PSVersion))"
# The checks above run native commands that fail on purpose (a wrong checksum), and
# GitHub's PowerShell steps exit with the last native exit code.
exit 0

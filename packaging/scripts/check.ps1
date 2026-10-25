<#
.SYNOPSIS
Dry run of the Windows packaging: PSScriptAnalyzer on the PowerShell scripts,
install.ps1's helpers, package.ps1, install.ps1 against a local release directory
in Windows PowerShell 5.1 and PowerShell 7, and the npm launcher. CI runs it on
Windows.

.DESCRIPTION
The archives hold a fake devlanes.exe: a copy of cmd.exe with the version appended
(Windows ignores bytes after the image), so it runs, can hold its own file locked,
and tells the installed versions apart. No Rust build is needed.

Each PowerShell installs a pinned version, then the latest one while the installed
devlanes.exe runs, which must be renamed to devlanes.exe.old-* and keep running,
then once more after it ended, which deletes the old copy. install.ps1 always puts
its directory in the user PATH, so the check saves the user PATH value and kind
first and writes them back at the end. Every child process has a timeout, after
which it is killed with its own children.

Needs PSScriptAnalyzer (Install-Module PSScriptAnalyzer -Scope CurrentUser), both
powershell.exe and pwsh, and node.

.EXAMPLE
pwsh packaging/scripts/check.ps1
#>
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingPositionalParameters', '', Justification = 'Assertions read better positionally.')]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0

$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$installScript = Join-Path $root 'install.ps1'
$packageScript = Join-Path $root 'packaging/scripts/package.ps1'
$work = Join-Path ([IO.Path]::GetTempPath()) ('devlanes-check-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
$running = $null

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
    param([string]$FilePath, [string[]]$Arguments, [int]$TimeoutSeconds = 300)
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
    <# Runs PowerShell code in a child powershell.exe or pwsh; returns exit code and output. #>
    param([string]$Shell, [string]$Command, [hashtable]$Environment)
    $Environment = $Environment.Clone()
    # pwsh's module path would make Windows PowerShell load PowerShell 7 modules;
    # without the variable it builds its own.
    if ([IO.Path]::GetFileNameWithoutExtension($Shell) -eq 'powershell') { $Environment.PSModulePath = $null }
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
        return Invoke-Process $Shell $arguments
    } finally {
        foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
    }
}

function Get-UserPath {
    <# The user PATH as stored: whether it exists, its unexpanded text and its kind. #>
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        if ($key.GetValueNames() -notcontains 'Path') { return @{ Exists = $false; Value = ''; Kind = $null } }
        return @{
            Exists = $true
            Value = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            Kind = $key.GetValueKind('Path')
        }
    } finally {
        $key.Dispose()
    }
}

function Restore-UserPath {
    <# Writes back a user PATH from Get-UserPath and tells running programs. #>
    param([hashtable]$Saved)
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        if ($Saved.Exists) {
            $key.SetValue('Path', $Saved.Value, $Saved.Kind)
        } elseif ($key.GetValueNames() -contains 'Path') {
            $key.DeleteValue('Path')
        }
    } finally {
        $key.Dispose()
    }
    # The same WM_SETTINGCHANGE broadcast install.ps1 sends.
    [Environment]::SetEnvironmentVariable('DEVLANES_INSTALL_PATH_CHANGED', '1', 'User')
    [Environment]::SetEnvironmentVariable('DEVLANES_INSTALL_PATH_CHANGED', $null, 'User')
}

function Get-FakeVersion {
    <# The version appended to a fake devlanes.exe. #>
    param([string]$Path)
    $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($Path))
    if ($text -match 'devlanes-check-version ([0-9a-z.-]+)$') { return $Matches[1] }
    return $null
}

# PSScriptAnalyzer's default rules, every severity.
Import-Module PSScriptAnalyzer
foreach ($script in @($installScript, $packageScript, $PSCommandPath)) {
    $findings = @(Invoke-ScriptAnalyzer -Path $script)
    if ($findings.Count -gt 0) {
        $findings | Format-Table -AutoSize -Wrap | Out-String | Write-Output
        throw "check.ps1: PSScriptAnalyzer findings in $script"
    }
}

foreach ($script in @($installScript, $packageScript, $PSCommandPath)) {
    $bytes = [IO.File]::ReadAllBytes($script)
    Assert-True (@($bytes | Where-Object { $_ -gt 127 }).Count -eq 0) "$script must be ASCII"
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
Assert-True ((Get-DevlanesArchitecture) -in @('AMD64', 'ARM64')) "architecture name $(Get-DevlanesArchitecture)"
Assert-DevlanesWindowsBuild 26100
$refused = $null
try { Assert-DevlanesWindowsBuild 22631 } catch { $refused = $_.Exception.Message }
Assert-Equal $refused 'devlanes install: devlanes needs Windows 11 24H2 (build 26100) or later, and this is build 22631' 'Windows 11 23H2 must be refused'
# The build install.ps1 checks is the one cmd.exe's `ver` shows.
$build = Get-DevlanesWindowsBuild
$ver = (& cmd.exe /d /c ver | Out-String).Trim()
# The word before the number is translated ("[Versione 10.0.26100.1]").
Assert-True ($ver -match '\[\D*\d+\.\d+\.(\d+)') "cmd ver output: $ver"
Assert-Equal $build ([int]$Matches[1]) 'Windows build'

$sums = "$('a' * 64)  devlanes-1.2.3-x86_64-unknown-linux-gnu.tar.gz`n$('b' * 64)  devlanes-1.2.3-x86_64-pc-windows-msvc.zip`n"
$found = Find-DevlanesArchive $sums 'x86_64-pc-windows-msvc' '' 'SHA256SUMS'
Assert-Equal $found.Version '1.2.3' 'version from SHA256SUMS'
Assert-Equal $found.Hash ('b' * 64) 'checksum from SHA256SUMS'
Assert-Equal $found.Archive 'devlanes-1.2.3-x86_64-pc-windows-msvc.zip' 'archive from SHA256SUMS'
foreach ($case in @(@('x86_64-pc-windows-msvc', '1.2.4'), @('aarch64-pc-windows-msvc', ''))) {
    $refused = $false
    try { Find-DevlanesArchive $sums $case[0] $case[1] 'SHA256SUMS' | Out-Null } catch { $refused = $true }
    Assert-True $refused "SHA256SUMS lookup of $($case -join ' ') must fail"
}

$shells = @(
    (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'),
    (Get-Command pwsh -CommandType Application | Select-Object -First 1).Source
)
$target = Get-DevlanesTarget (Get-DevlanesArchitecture)
$versions = @('0.0.1-check', '0.0.2-check')
$pathBefore = Get-UserPath
New-Item -ItemType Directory -Path $work | Out-Null
try {
    # A devlanes.exe ahead of the install directory is reported; one behind it is not.
    $shadowing = Join-Path $work 'shadowing'
    $behind = Join-Path $work 'behind'
    New-Item -ItemType Directory -Path $shadowing, $behind | Out-Null
    Set-Content -LiteralPath (Join-Path $shadowing 'devlanes.cmd') -Value '@exit /b 0'
    $shadow = Find-DevlanesShadow "C:\acme<>;$shadowing\;$behind" $behind
    Assert-True ($shadow -ieq "$shadowing\devlanes.cmd") "devlanes before the install directory: $shadow"
    Assert-Equal (Find-DevlanesShadow "$behind;$shadowing" $behind) $null 'devlanes after the install directory'

    # The release layout install.ps1 downloads from, with two versions; latest is the second.
    $release = Join-Path $work 'release'
    $latest = Join-Path $release 'latest/download'
    New-Item -ItemType Directory -Force -Path $latest | Out-Null
    $lines = @{}
    foreach ($version in $versions) {
        $bin = Join-Path $work "bin-$version"
        New-Item -ItemType Directory -Path $bin | Out-Null
        $fake = [IO.File]::ReadAllBytes((Join-Path $env:SystemRoot 'System32\cmd.exe')) +
            [Text.Encoding]::ASCII.GetBytes("devlanes-check-version $version")
        [IO.File]::WriteAllBytes((Join-Path $bin 'devlanes.exe'), $fake)
        $name = "devlanes-$version-$target"
        # The same inputs must give the same bytes with the same PowerShell (the two
        # ship different zlib builds). The release uses pwsh's, the last one.
        $hashes = foreach ($shell in $shells) { foreach ($run in 1, 2) {
            $out = Join-Path $work "dist-$version-$([IO.Path]::GetFileNameWithoutExtension($shell))-$run"
            $package = Invoke-Process $shell @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $packageScript,
                '-Version', $version, '-Target', $target, '-OutDir', $out, '-BinDir', $bin)
            Assert-Equal $package.Code 0 "package.ps1 exit code: $($package.Output)"
            $zip = Join-Path $out "$name.zip"
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $archive = [IO.Compression.ZipFile]::OpenRead($zip)
            try {
                $entries = @($archive.Entries | ForEach-Object { $_.FullName }) -join ' '
            } finally {
                $archive.Dispose()
            }
            Assert-Equal $entries "$name/LICENSE $name/devlanes.exe" 'archive entries'
            (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        } }
        Assert-Equal $hashes[1] $hashes[0] "$name.zip from a second Windows PowerShell run"
        Assert-Equal $hashes[3] $hashes[2] "$name.zip from a second pwsh run"
        $hashes = @($hashes[3])
        $versioned = Join-Path $release "download/v$version"
        New-Item -ItemType Directory -Force -Path $versioned | Out-Null
        Copy-Item -LiteralPath $zip -Destination $versioned
        # A Linux archive in the same file, as in a real release.
        $lines[$version] = "$('0' * 64)  devlanes-$version-x86_64-unknown-linux-gnu.tar.gz`n$($hashes[0])  $name.zip`n"
        [IO.File]::WriteAllText((Join-Path $versioned 'SHA256SUMS'), $lines[$version])
    }
    [IO.File]::WriteAllText((Join-Path $latest 'SHA256SUMS'), $lines[$versions[1]])
    # `next` holds the older version, so installing from it is visible as a downgrade.
    $next = Join-Path $release 'download/next'
    Copy-Item -LiteralPath (Join-Path $release "download/v$($versions[0])") -Destination $next -Recurse

    foreach ($shell in $shells) {
        $label = [IO.Path]::GetFileNameWithoutExtension($shell)
        $installed = Join-Path $work "installed by $label"
        $exe = Join-Path $installed 'devlanes.exe'
        $environment = @{
            DEVLANES_INSTALL_BASE_URL = ([Uri]$release).AbsoluteUri
            DEVLANES_INSTALL_DIR = $installed
            DEVLANES_VERSION = "v$($versions[0])"
        }

        $run = Invoke-Child $shell "& '$installScript'" $environment
        Assert-Equal $run.Code 0 "$label, install.ps1 as a file: $($run.Output)"
        $present = @(Get-ChildItem -LiteralPath $installed -Force | ForEach-Object { $_.Name }) -join ' '
        Assert-Equal $present 'devlanes.exe' "$label, installed files"
        Assert-Equal (Get-FakeVersion $exe) $versions[0] "$label, installed version"
        Assert-True ($run.Output.Contains("Installed devlanes $($versions[0]) to $installed")) "$label, install message: $($run.Output)"
        $path = Get-UserPath
        Assert-True ($path.Value.StartsWith("$installed;") -or $path.Value -eq $installed) "$label, user PATH starts with the directory: $($path.Value)"
        if ($pathBefore.Exists) {
            Assert-Equal $path.Kind $pathBefore.Kind "$label, user PATH value kind"
        } else {
            Assert-Equal $path.Kind ([Microsoft.Win32.RegistryValueKind]::ExpandString) "$label, new user PATH value kind"
        }

        # The installed file runs and stays locked while it does, as a daemon would.
        $start = New-Object Diagnostics.ProcessStartInfo $exe
        $start.Arguments = '/d /c ping -n 120 127.0.0.1 >nul'
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $running = [Diagnostics.Process]::Start($start)

        # `irm | iex` runs the text in the caller's session, which must keep its own
        # variables and gain no functions or variables; the latest release this time.
        $environment.DEVLANES_VERSION = $null
        $before = '$version = $dir = ''mine''
$names = @(Get-Variable | ForEach-Object Name) + @(Get-ChildItem Function: | ForEach-Object Name)'
        $after = '$left = @(Get-Variable | ForEach-Object Name) + @(Get-ChildItem Function: | ForEach-Object Name) | Where-Object { $names -notcontains $_ -and $_ -ne ''names'' }
if ($left) { throw "left in the session: $($left -join '', '')" }
if (@($version, $dir) -ne ''mine'') { throw ''the session lost its own $version or $dir'' }'
        $run = Invoke-Child $shell "$before`nGet-Content -Raw -LiteralPath '$installScript' | Invoke-Expression`n$after" $environment
        Assert-Equal $run.Code 0 "$label, install.ps1 through Invoke-Expression over a running devlanes.exe: $($run.Output)"
        Assert-Equal (Get-FakeVersion $exe) $versions[1] "$label, upgraded version"
        $old = @(Get-ChildItem -LiteralPath $installed -Filter 'devlanes.exe.old-*')
        Assert-Equal $old.Count 1 "$label, renamed running executable"
        Assert-Equal (Get-FakeVersion $old[0].FullName) $versions[0] "$label, version of the renamed executable"
        Assert-True (-not $running.HasExited) "$label, the running devlanes.exe keeps running through the upgrade"
        Assert-True ($run.Output.Contains('devlanes daemon restart')) "$label, restart advice: $($run.Output)"
        Assert-True (-not $run.Output.Contains('Added ')) "$label, the user PATH already had the directory: $($run.Output)"

        & taskkill.exe /PID $running.Id /T /F 2>&1 | Out-Null
        Assert-True ($running.WaitForExit(30000)) "$label, the fake daemon exits"
        $running.Dispose()
        $running = $null
        # Windows may release the image file a moment after the process ended.
        $deadline = (Get-Date).AddSeconds(30)
        while ($true) {
            try {
                [IO.File]::Open($old[0].FullName, 'Open', 'ReadWrite', 'None').Dispose()
                break
            } catch {
                if ((Get-Date) -gt $deadline) { throw "check.ps1: $($old[0].FullName) stays locked: $($_.Exception.Message)" }
                Start-Sleep -Milliseconds 200
            }
        }
        $run = Invoke-Child $shell "& '$installScript'" $environment
        Assert-Equal $run.Code 0 "$label, install.ps1 after the old copy stopped: $($run.Output)"
        Assert-Equal @(Get-ChildItem -LiteralPath $installed -Filter 'devlanes.exe.old-*').Count 0 "$label, old copies are deleted"

        $sumsFile = Join-Path $latest 'SHA256SUMS'
        [IO.File]::WriteAllText($sumsFile, $lines[$versions[1]].Replace(($lines[$versions[1]] -split '\s+')[2], 'f' * 64))
        $run = Invoke-Child $shell "& '$installScript'" $environment
        Assert-True ($run.Code -ne 0 -and $run.Output.Contains('checksum mismatch')) "$label, a wrong checksum must fail: $($run.Output)"
        Assert-Equal (Get-FakeVersion $exe) $versions[1] "$label, a failed install keeps the installed file"
        [IO.File]::WriteAllText($sumsFile, $lines[$versions[1]])

        # The rolling `next` release keeps its archives under download/next.
        $environment.DEVLANES_VERSION = 'next'
        $run = Invoke-Child $shell "& '$installScript'" $environment
        Assert-Equal $run.Code 0 "$label, install.ps1 from next: $($run.Output)"
        Assert-Equal (Get-FakeVersion $exe) $versions[0] "$label, version from next"

        # The same through options, as the documented `iex "& {$(irm ...)} -Channel next"`
        # passes them; an option wins over the environment variable.
        $withOptions = { param([string]$Options) "iex `"& {`$(Get-Content -Raw -LiteralPath '$installScript')} $Options`"" }
        $run = Invoke-Child $shell (& $withOptions '-Channel stable') $environment
        Assert-Equal $run.Code 0 "$label, -Channel stable: $($run.Output)"
        Assert-Equal (Get-FakeVersion $exe) $versions[1] "$label, -Channel stable over DEVLANES_VERSION=next"
        $environment.DEVLANES_VERSION = $null
        $run = Invoke-Child $shell (& $withOptions '-Channel next') $environment
        Assert-Equal (Get-FakeVersion $exe) $versions[0] "$label, -Channel next: $($run.Output)"
        # Running the text as a script block passes options the same way.
        $run = Invoke-Child $shell "& ([scriptblock]::Create((Get-Content -Raw -LiteralPath '$installScript'))) -Version v$($versions[1])" $environment
        Assert-Equal (Get-FakeVersion $exe) $versions[1] "$label, -Version: $($run.Output)"
        $elsewhere = Join-Path $work "installed elsewhere by $label"
        $run = Invoke-Child $shell (& $withOptions "-Version $($versions[0]) -Dir '$elsewhere'") $environment
        Assert-Equal (Get-FakeVersion (Join-Path $elsewhere 'devlanes.exe')) $versions[0] "$label, -Dir: $($run.Output)"
        # Windows PowerShell exits with 0 after a parameter validation error, so the check
        # is the error text and an untouched installation.
        foreach ($wrong in @('-Channel dev', "-Channel next -Version $($versions[0])")) {
            $run = Invoke-Child $shell (& $withOptions $wrong) $environment
            Assert-True ($run.Output -match 'Cannot validate argument|can''t be combined') "$label, install.ps1 accepted $($wrong): $($run.Output)"
            Assert-Equal (Get-FakeVersion $exe) $versions[1] "$label, $wrong left the installation alone"
        }

        Restore-UserPath $pathBefore
    }

    # The npm launcher finds the platform package's devlanes.exe and exits with its code.
    $arch = (& node -p 'process.arch' | Out-String).Trim()
    $modules = Join-Path $work 'npm/node_modules'
    $launcherDir = Join-Path $modules 'devlanes/bin'
    $platformDir = Join-Path $modules "@devlanes/win32-$arch"
    New-Item -ItemType Directory -Force -Path $launcherDir, (Join-Path $platformDir 'bin') | Out-Null
    Copy-Item -LiteralPath (Join-Path $root 'packaging/npm/devlanes/bin/devlanes') -Destination $launcherDir
    [IO.File]::WriteAllText((Join-Path $platformDir 'package.json'), "{`"name`": `"@devlanes/win32-$arch`", `"version`": `"$($versions[1])`"}")
    Copy-Item -LiteralPath (Join-Path $work "bin-$($versions[1])/devlanes.exe") -Destination (Join-Path $platformDir 'bin')
    $node = (Get-Command node -CommandType Application | Select-Object -First 1).Source
    $launch = Invoke-Process $node @((Join-Path $launcherDir 'devlanes'), '/d', '/c', 'exit 7')
    Assert-Equal $launch.Code 7 "npm launcher exit code: $($launch.Output)"
    Remove-Item -LiteralPath $platformDir -Recurse -Force
    $launch = Invoke-Process $node @((Join-Path $launcherDir 'devlanes'), '--version')
    Assert-True ($launch.Code -eq 1 -and $launch.Output.Contains('win32-arm64 and win32-x64')) "npm launcher without a platform package: $($launch.Output)"
} finally {
    if ($running -and -not $running.HasExited) { & taskkill.exe /PID $running.Id /T /F 2>&1 | Out-Null }
    Restore-UserPath $pathBefore
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
$pathAfter = Get-UserPath
Assert-True ($pathAfter.Exists -eq $pathBefore.Exists -and $pathAfter.Value -ceq $pathBefore.Value -and $pathAfter.Kind -eq $pathBefore.Kind) 'the user PATH is restored'
Write-Output "Windows packaging: dry run passed ($($PSVersionTable.PSEdition) $($PSVersionTable.PSVersion))"
# The checks above run native commands that fail on purpose (a wrong checksum), and
# GitHub's PowerShell steps exit with the last native exit code.
exit 0

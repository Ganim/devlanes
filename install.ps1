<#
.SYNOPSIS
Installs devlanes for Windows from a GitHub release after checking the archive
against the release's SHA256SUMS.

.DESCRIPTION
    irm https://github.com/devlanes-run/devlanes/releases/latest/download/install.ps1 | iex

`irm | iex` passes no arguments, so every setting is also an environment variable:

    DEVLANES_VERSION=0.1.0       pins a release (default: the latest one)
    DEVLANES_INSTALL_DIR=DIR     sets the target directory (default: %LOCALAPPDATA%\Programs\devlanes)
    DEVLANES_NO_MODIFY_PATH=1    leaves the user PATH alone
    DEVLANES_INSTALL_BASE_URL    replaces the release URL; it exists only so the script can be
                                 tested against local releases, and accepts http:// and file:// URLs

Run as a file, -Version, -InstallDir and -NoModifyPath do the same. A devlanes.exe
that is running (usually the daemon) is renamed to devlanes.exe.old-TIMESTAMP and
keeps running; `devlanes daemon restart` switches to the new version and
`devlanes doctor` deletes the old file afterwards. The script never stops the daemon.
Works in Windows PowerShell 5.1 and PowerShell 7.
#>
# `irm | iex` binds these parameters in the caller's session and overwrites variables
# of the same names there, hence the long names; the aliases are the documented ones.
param(
    [Alias('Version')][string]$DevlanesInstallVersion = $env:DEVLANES_VERSION,
    [Alias('InstallDir')][string]$DevlanesInstallDir = $env:DEVLANES_INSTALL_DIR,
    [Alias('NoModifyPath')][switch]$DevlanesNoModifyPath
)

# `irm | iex` also runs this text in the caller's session, so the body is a script
# block called with `&`: its variables and functions end with it, and `exit` (which
# would close the window) is never used. Failures throw, which ends a file run with
# exit code 1 and leaves an iex session open.
& {
    param([string]$Version, [string]$InstallDir, [bool]$NoModifyPath)

    # The files of every Windows release archive, below its devlanes-VERSION-TARGET\ directory.
    $DevlanesFiles = @('LICENSE', 'devlanes-hosts-service.exe', 'devlanes.exe', 'devlanesw.exe')

    function Get-DevlanesArchitecture {
        <# The machine's architecture name (AMD64, ARM64, x86). #>
        # The process value says AMD64 inside x64 PowerShell emulated on an arm64 machine and
        # x86 in 32-bit PowerShell; the machine value in the registry names the hardware.
        $machine = [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE', 'Machine')
        if ($machine) { return $machine }
        if ($env:PROCESSOR_ARCHITEW6432) { return $env:PROCESSOR_ARCHITEW6432 }
        return $env:PROCESSOR_ARCHITECTURE
    }

    function Get-DevlanesTarget {
        <# The release target for an architecture name. #>
        param([string]$Architecture)
        switch ($Architecture) {
            'AMD64' { return 'x86_64-pc-windows-msvc' }
            'ARM64' { return 'aarch64-pc-windows-msvc' }
            default { throw "devlanes install: no release build for Windows $Architecture" }
        }
    }

    function Find-DevlanesArchive {
        <# The archive line for Target in a SHA256SUMS text: Hash, Archive and Version. #>
        param([string]$Sums, [string]$Target, [string]$Version, [string]$Source)
        $suffix = "-$Target.zip"
        foreach ($line in ($Sums -split "`r?`n")) {
            $fields = @($line -split '\s+' | Where-Object { $_ -ne '' })
            if ($fields.Count -ne 2) { continue }
            $archive = $fields[1].TrimStart('*')
            if (-not $archive.StartsWith('devlanes-') -or -not $archive.EndsWith($suffix)) { continue }
            $found = $archive.Substring('devlanes-'.Length, $archive.Length - 'devlanes-'.Length - $suffix.Length)
            if ($Version -and $found -ne $Version) {
                throw "devlanes install: $Source lists version $found, expected $Version"
            }
            if ($found -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$') {
                throw "devlanes install: unexpected version $found in $Source"
            }
            $hash = $fields[0].ToLowerInvariant()
            if ($hash -notmatch '^[0-9a-f]{64}$') {
                throw "devlanes install: malformed checksum for $archive"
            }
            return @{ Hash = $hash; Archive = $archive; Version = $found }
        }
        throw "devlanes install: no $Target archive listed in $Source"
    }

    function Get-DevlanesDownload {
        <# Downloads Url to OutFile; only the listed URL schemes are accepted. #>
        param([string]$Url, [string]$OutFile, [string[]]$Schemes)
        $uri = [Uri]$Url
        if ($Schemes -notcontains $uri.Scheme) {
            throw "devlanes install: refusing $Url (allowed: $($Schemes -join ', '))"
        }
        try {
            if ($uri.Scheme -eq 'file') {
                Copy-Item -LiteralPath $uri.LocalPath -Destination $OutFile
            } else {
                Invoke-WebRequest -Uri $uri -OutFile $OutFile -UseBasicParsing
            }
        } catch {
            throw "devlanes install: cannot download ${Url}: $($_.Exception.Message)"
        }
    }

    function Get-DevlanesPathDirectory {
        <# A PATH entry as a directory to compare: variables expanded, quotes and trailing \ removed. #>
        param([string]$Entry)
        return [Environment]::ExpandEnvironmentVariables($Entry.Trim().Trim('"')).TrimEnd('\')
    }

    function Test-DevlanesPathEntry {
        <# Whether a PATH value has an entry naming Directory (case and trailing \ ignored). #>
        param([AllowEmptyString()][string]$PathValue, [string]$Directory)
        $wanted = $Directory.Trim().TrimEnd('\')
        foreach ($entry in ($PathValue -split ';')) {
            $expanded = Get-DevlanesPathDirectory $entry
            if ($expanded -and $expanded -ieq $wanted) { return $true }
        }
        return $false
    }

    function Find-DevlanesShadow {
        <# The first devlanes command (any PATHEXT extension) in a PATH entry before Directory, or $null. #>
        param([AllowEmptyString()][string]$PathValue, [string]$Directory)
        $wanted = $Directory.Trim().TrimEnd('\')
        $extensions = @(([string]$env:PATHEXT -split ';') | Where-Object { $_ })
        if ($extensions.Count -eq 0) { $extensions = @('.COM', '.EXE', '.BAT', '.CMD') }
        foreach ($entry in ($PathValue -split ';')) {
            $expanded = Get-DevlanesPathDirectory $entry
            if (-not $expanded) { continue }
            if ($expanded -ieq $wanted) { return $null }
            foreach ($extension in $extensions) {
                # File.Exists, unlike Join-Path and Test-Path, returns false for entries
                # with characters a path cannot hold.
                $candidate = "$expanded\devlanes$extension"
                if ([IO.File]::Exists($candidate)) { return $candidate }
            }
        }
        return $null
    }

    function Add-DevlanesPathEntry {
        <# PathValue with Directory in front, or $null when an entry already names it. #>
        param([AllowEmptyString()][string]$PathValue, [string]$Directory)
        if (Test-DevlanesPathEntry $PathValue $Directory) { return $null }
        # The rest stays byte for byte, %VARIABLE% references included.
        if ($PathValue) { return "$Directory;$PathValue" }
        return $Directory
    }

    function Add-DevlanesUserPath {
        <# Puts Directory first in the user PATH; returns whether the value changed. #>
        param([string]$Directory)
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
        try {
            # Read unexpanded and write back as REG_EXPAND_SZ, so entries like
            # %USERPROFILE%\bin keep working; [Environment]::SetEnvironmentVariable would
            # store the expanded text as REG_SZ.
            $current = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $updated = Add-DevlanesPathEntry ([string]$current) $Directory
            if ($null -eq $updated) { return $false }
            $key.SetValue('Path', $updated, [Microsoft.Win32.RegistryValueKind]::ExpandString)
        } finally {
            $key.Dispose()
        }
        # Setting a user variable through .NET broadcasts WM_SETTINGCHANGE, so Explorer and
        # terminals started from it afterwards see the new PATH; the registry write alone
        # does not. The variable is removed again at once.
        [Environment]::SetEnvironmentVariable('DEVLANES_INSTALL_PATH_CHANGED', '1', 'User')
        [Environment]::SetEnvironmentVariable('DEVLANES_INSTALL_PATH_CHANGED', $null, 'User')
        return $true
    }

    function Install-DevlanesFile {
        <#
        Renames Staged to Directory\Name. An existing file is renamed to Name.old-Stamp first,
        which Windows allows even while the file runs, and deleted when nothing holds it.
        Returns the old file's path when it is still in use, else $null.
        #>
        param([string]$Staged, [string]$Directory, [string]$Name, [string]$Stamp)
        $target = Join-Path $Directory $Name
        $old = $null
        if (Test-Path -LiteralPath $target) {
            $old = "$target.old-$Stamp"
            $n = 1
            while (Test-Path -LiteralPath $old) { $old = "$target.old-$Stamp-$n"; $n++ }
            Move-Item -LiteralPath $target -Destination $old
        }
        try {
            Move-Item -LiteralPath $Staged -Destination $target
        } catch {
            if ($old) { Move-Item -LiteralPath $old -Destination $target }
            throw
        }
        if (-not $old) { return $null }
        try {
            Remove-Item -LiteralPath $old -Force
            return $null
        } catch {
            return $old
        }
    }

    function Install-DevlanesRelease {
        <# Downloads, checks and installs a release; see the script's help for the settings. #>
        param([string]$Version, [string]$InstallDir, [switch]$NoModifyPath)
        $ErrorActionPreference = 'Stop'
        # Windows PowerShell's progress bar slows Invoke-WebRequest down by an order of magnitude.
        $ProgressPreference = 'SilentlyContinue'
        Set-StrictMode -Version 3.0

        # Windows PowerShell 5.1 may still default to TLS 1.0 and 1.1, which GitHub refuses.
        if ($PSVersionTable.PSVersion.Major -lt 6) {
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        }

        $base = 'https://github.com/devlanes-run/devlanes/releases'
        $schemes = @('https')
        if ($env:DEVLANES_INSTALL_BASE_URL) {
            $base = $env:DEVLANES_INSTALL_BASE_URL.TrimEnd('/')
            $schemes = @('https', 'http', 'file')
        }
        if ($Version) { $Version = $Version.TrimStart('v') }
        if (-not $InstallDir) {
            if (-not $env:LOCALAPPDATA) { throw 'devlanes install: LOCALAPPDATA is not set; set DEVLANES_INSTALL_DIR' }
            $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\devlanes'
        }
        # Relative to PowerShell's location, which .NET's current directory does not follow.
        $InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir)

        $target = Get-DevlanesTarget (Get-DevlanesArchitecture)
        $temporary = Join-Path ([IO.Path]::GetTempPath()) ("devlanes-install-" + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $temporary | Out-Null
        try {
            # The latest release is found through its SHA256SUMS, which names the archive and
            # so the version; the archive then comes from that version's own release.
            if ($Version) {
                $sumsUrl = "$base/download/v$Version/SHA256SUMS"
            } else {
                $sumsUrl = "$base/latest/download/SHA256SUMS"
            }
            $sumsFile = Join-Path $temporary 'SHA256SUMS'
            Get-DevlanesDownload $sumsUrl $sumsFile $schemes
            $release = Find-DevlanesArchive ([IO.File]::ReadAllText($sumsFile)) $target $Version $sumsUrl
            $Version = $release.Version

            $url = "$base/download/v$Version/$($release.Archive)"
            $archive = Join-Path $temporary $release.Archive
            Get-DevlanesDownload $url $archive $schemes
            $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actual -ne $release.Hash) {
                throw "devlanes install: checksum mismatch for $($release.Archive): expected $($release.Hash), got $actual"
            }

            # Extract only the exact entries release archives contain.
            $top = "devlanes-$Version-$target"
            $expected = @($DevlanesFiles | ForEach-Object { "$top/$_" })
            $extracted = Join-Path $temporary 'files'
            New-Item -ItemType Directory -Path $extracted | Out-Null
            Add-Type -AssemblyName System.IO.Compression
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $zip = [IO.Compression.ZipFile]::OpenRead($archive)
            try {
                $names = @($zip.Entries | ForEach-Object { $_.FullName } | Where-Object { $_ -ne "$top/" } | Sort-Object)
                if (($names -join '|') -cne (($expected | Sort-Object) -join '|')) {
                    throw "devlanes install: unexpected entries in $($release.Archive): $($names -join ', ')"
                }
                foreach ($entry in $zip.Entries) {
                    if ($expected -ccontains $entry.FullName) {
                        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $extracted $entry.Name))
                    }
                }
            } finally {
                $zip.Dispose()
            }

            New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
            $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddHHmmss')
            $inUse = @()
            # Every file is copied next to its target before the first one is replaced, so
            # a full disk or a failed copy leaves the installed version untouched and the
            # replacing is renames on one volume.
            $staged = @()
            try {
                foreach ($name in $DevlanesFiles) {
                    $path = Join-Path $InstallDir ".$name.new"
                    $staged += $path
                    Copy-Item -LiteralPath (Join-Path $extracted $name) -Destination $path -Force
                }
                foreach ($name in $DevlanesFiles) {
                    $old = Install-DevlanesFile (Join-Path $InstallDir ".$name.new") $InstallDir $name $stamp
                    if ($old) { $inUse += $old }
                }
            } finally {
                # Only a failed copy or rename leaves staged files behind.
                foreach ($path in $staged) { Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue }
            }
        } finally {
            Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
        }

        Write-Host "Installed devlanes $Version to $InstallDir"
        if ($inUse.Count -gt 0) {
            Write-Host 'The previous version is still running, probably as the daemon, and keeps running until it stops:'
            foreach ($old in $inUse) { Write-Host "  $old" }
            Write-Host 'Run `devlanes daemon restart` when convenient (it stops services); `devlanes doctor` then deletes the old files.'
        }

        $onPath = Test-DevlanesPathEntry $env:Path $InstallDir
        if ($NoModifyPath) {
            if (-not $onPath) {
                Write-Host "$InstallDir is not on PATH. Add it to your user PATH, or run it as $(Join-Path $InstallDir 'devlanes.exe')."
            }
        } else {
            if (Add-DevlanesUserPath $InstallDir) {
                Write-Host "Added $InstallDir to your user PATH; terminals opened from now on find devlanes."
            }
            if (-not $onPath) {
                $env:Path = "$InstallDir;$env:Path"
            }
        }
        # New terminals get the machine PATH followed by the user PATH, so a devlanes in
        # the machine PATH (or before this directory in the user PATH) runs instead.
        $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $shadow = Find-DevlanesShadow "$machinePath;$userPath" $InstallDir
        if ($shadow) {
            Write-Host "Another devlanes comes first on PATH: $shadow"
        }
    }

    Install-DevlanesRelease -Version $Version -InstallDir $InstallDir -NoModifyPath:$NoModifyPath
} $DevlanesInstallVersion $DevlanesInstallDir ($DevlanesNoModifyPath -or $env:DEVLANES_NO_MODIFY_PATH -eq '1')

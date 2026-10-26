# Installs devlanes for Windows from a GitHub release after checking the archive
# against the release's SHA256SUMS.
#
#   irm https://devlanes.run/install.ps1 | iex
#   iex "& {$(irm https://devlanes.run/install.ps1)} -Channel next"
#   iex "& {$(irm https://devlanes.run/install.ps1)} -Version 0.4.0"
#
# -Channel stable (the default) installs the latest release, -Channel next the latest
# build of main from the rolling `next` prerelease, and -Version X.Y.Z one release.
# -Dir DIR sets the target directory (default: %LOCALAPPDATA%\Programs\devlanes).
# `irm | iex` passes no arguments, so $env:DEVLANES_VERSION (X.Y.Z or next) and
# $env:DEVLANES_INSTALL_DIR, the names install.sh reads, do the same when the option
# isn't given.
# DEVLANES_INSTALL_BASE_URL replaces the release URL; it exists only so the script
# can be tested against a local directory of fake releases (file:// URLs).
#
# The directory goes first in the user PATH. A devlanes.exe that is running
# (usually the daemon) is renamed to devlanes.exe.old-TIMESTAMP and keeps running;
# `devlanes daemon restart` switches to the new version and `devlanes doctor`
# deletes the old file afterwards. The script never stops the daemon. It works in
# Windows PowerShell 5.1 and PowerShell 7, on Windows 11 24H2 (build 26100) or later.

# `irm | iex` runs this text in the caller's session, so the body is a script block
# called with `&`: its variables and functions end with it, and `exit` (which would
# close the window) is never used. Failures throw, which ends a file run with exit
# code 1 and leaves an iex session open. The options are the block's parameters, so
# they never reach the caller's session either. `@args` forwards them by name from the
# `& { ... } -Channel next` wrapper above, and is empty under plain `irm | iex`.
& {
    param(
        [ValidateSet('stable', 'next')][string]$Channel,
        [string]$Version,
        [string]$Dir
    )
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell's progress bar slows Invoke-WebRequest down by an order of magnitude.
    $ProgressPreference = 'SilentlyContinue'
    Set-StrictMode -Version 3.0

    # The files of every Windows release archive, below its devlanes-VERSION-TARGET/ directory.
    $DevlanesFiles = @('LICENSE', 'devlanes.exe')

    function Get-DevlanesWindowsBuild {
        <#
        The Windows build from the registry. [Environment]::OSVersion comes from
        GetVersionEx, whose answer depends on the host program's manifest.
        #>
        [int](Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -Name CurrentBuildNumber).CurrentBuildNumber
    }

    function Assert-DevlanesWindowsBuild {
        <# Throws when Build is older than Windows 11 24H2 (build 26100), the oldest devlanes supports. #>
        param([int]$Build)
        $minimum = 26100
        if ($Build -lt $minimum) {
            throw "devlanes install: devlanes needs Windows 11 24H2 (build $minimum) or later, and this is build $Build"
        }
    }

    function Get-DevlanesArchitecture {
        <# The machine's architecture name (AMD64, ARM64, x86). #>
        # The process value says AMD64 inside x64 PowerShell emulated on an arm64 machine
        # and x86 in 32-bit PowerShell; the machine value in the registry names the hardware.
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
            $archive = $fields[1]
            if (-not $archive.StartsWith('devlanes-') -or -not $archive.EndsWith($suffix)) { continue }
            # The latest release is found through its SHA256SUMS, which names the archive
            # and so the version.
            $found = $archive.Substring('devlanes-'.Length, $archive.Length - 'devlanes-'.Length - $suffix.Length)
            if ($Version -and $found -ne $Version) {
                throw "devlanes install: $Source lists version $found, expected $Version"
            }
            if ($found -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$') {
                throw "devlanes install: unexpected version $found in $Source"
            }
            $hash = $fields[0]
            if ($hash -cnotmatch '^[0-9a-f]{64}$') {
                throw "devlanes install: malformed checksum for $archive"
            }
            return @{ Hash = $hash; Archive = $archive; Version = $found }
        }
        throw "devlanes install: no $Target archive listed in $Source"
    }

    function Get-DevlanesFileHash {
        <# The lowercase hex SHA-256 of a file. #>
        param([string]$Path)
        # Not Get-FileHash: Windows PowerShell defines it in its own Utility module, and
        # with a module path inherited from pwsh (or a shell pwsh started, such as Git
        # Bash) it loads PowerShell 7's Utility module instead, which lacks it.
        $sha = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($Path)
        try {
            return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
        } finally {
            $stream.Dispose()
            $sha.Dispose()
        }
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
            # Read unexpanded and written back with the same kind, so entries like
            # %USERPROFILE%\bin in a REG_EXPAND_SZ value keep working;
            # [Environment]::SetEnvironmentVariable would store expanded text as REG_SZ.
            $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
            $current = ''
            if ($key.GetValueNames() -contains 'Path') {
                $kind = $key.GetValueKind('Path')
                $current = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            }
            $updated = Add-DevlanesPathEntry $current $Directory
            if ($null -eq $updated) { return $false }
            $key.SetValue('Path', $updated, $kind)
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

    function Clear-DevlanesOldCopy {
        <# Deletes the devlanes.exe.old-* files in Directory that nothing runs any more. #>
        param([string]$Directory)
        foreach ($old in @(Get-ChildItem -LiteralPath $Directory -Filter 'devlanes.exe.old-*' -File -ErrorAction SilentlyContinue)) {
            Remove-Item -LiteralPath $old.FullName -Force -ErrorAction SilentlyContinue
        }
    }

    function Install-DevlanesFile {
        <#
        Renames Staged to Directory\Name. An existing file is renamed to Name.old-STAMP
        first, which Windows allows even while the file runs, and deleted when nothing
        holds it. Returns the old file's path when it is still in use, else $null.
        #>
        param([string]$Staged, [string]$Directory, [string]$Name)
        $target = Join-Path $Directory $Name
        $old = $null
        if (Test-Path -LiteralPath $target) {
            $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddHHmmss')
            $old = "$target.old-$stamp"
            $n = 1
            while (Test-Path -LiteralPath $old) { $old = "$target.old-$stamp-$n"; $n++ }
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
        <# Downloads, checks and installs a release; see the comment at the top for the settings. #>
        [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '', Justification = 'An installer reports to the console, not to the pipeline.')]
        param([string]$Channel, [string]$Version, [string]$Dir)
        if ($Channel -and $Version) { throw "devlanes install: -Channel and -Version can't be combined" }
        Assert-DevlanesWindowsBuild (Get-DevlanesWindowsBuild)
        $target = Get-DevlanesTarget (Get-DevlanesArchitecture)

        $base = 'https://github.com/devlanes-run/devlanes/releases'
        $schemes = @('https')
        if ($env:DEVLANES_INSTALL_BASE_URL) {
            $base = $env:DEVLANES_INSTALL_BASE_URL.TrimEnd('/')
            $schemes = @('https', 'file')
        }
        if ($Channel -eq 'next') {
            $version = 'next'
        } elseif ($Channel -or $Version) {
            $version = $Version
        } else {
            $version = [string]$env:DEVLANES_VERSION
        }
        if ($version.StartsWith('v')) { $version = $version.Substring(1) }
        # `next` is the rolling prerelease with the latest build of main.
        $channel = ''
        if ($version -eq 'next') {
            $channel = 'next'
            $version = ''
        }
        # $dir is the -Dir parameter (PowerShell names are case-insensitive).
        if (-not $dir) { $dir = [string]$env:DEVLANES_INSTALL_DIR }
        if (-not $dir) {
            if (-not $env:LOCALAPPDATA) { throw 'devlanes install: LOCALAPPDATA is not set; pass -Dir' }
            $dir = Join-Path $env:LOCALAPPDATA 'Programs\devlanes'
        }
        # Relative to PowerShell's location, which .NET's current directory does not follow.
        $dir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($dir)

        $temporary = Join-Path ([IO.Path]::GetTempPath()) ('devlanes-install-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $temporary | Out-Null
        $staged = Join-Path $dir ".devlanes.exe.new.$PID"
        try {
            if ($channel) {
                $sumsUrl = "$base/download/$channel/SHA256SUMS"
            } elseif ($version) {
                $sumsUrl = "$base/download/v$version/SHA256SUMS"
            } else {
                $sumsUrl = "$base/latest/download/SHA256SUMS"
            }
            $sumsFile = Join-Path $temporary 'SHA256SUMS'
            Get-DevlanesDownload -Url $sumsUrl -OutFile $sumsFile -Schemes $schemes
            $release = Find-DevlanesArchive -Sums ([IO.File]::ReadAllText($sumsFile)) -Target $target -Version $version -Source $sumsUrl
            $version = $release.Version

            # The archive comes from that version's own release, or from `next`.
            $folder = if ($channel) { $channel } else { "v$version" }
            $url = "$base/download/$folder/$($release.Archive)"
            $archive = Join-Path $temporary $release.Archive
            Get-DevlanesDownload -Url $url -OutFile $archive -Schemes $schemes
            $actual = Get-DevlanesFileHash $archive
            if ($actual -ne $release.Hash) {
                throw "devlanes install: checksum mismatch for $($release.Archive): expected $($release.Hash), got $actual"
            }

            # Only the exact entries release archives contain. devlanes.exe is extracted
            # straight next to its target, so replacing it is a rename on one volume and
            # a full disk or a failed write leaves the installed version untouched.
            $top = "devlanes-$version-$target"
            $expected = @($DevlanesFiles | ForEach-Object { "$top/$_" })
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
            Add-Type -AssemblyName System.IO.Compression
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $zip = [IO.Compression.ZipFile]::OpenRead($archive)
            try {
                $names = @($zip.Entries | ForEach-Object { $_.FullName } | Where-Object { $_ -ne "$top/" } | Sort-Object)
                if (($names -join '|') -cne (($expected | Sort-Object) -join '|')) {
                    throw "devlanes install: unexpected entries in $($release.Archive): $($names -join ', ')"
                }
                $entry = $zip.GetEntry("$top/devlanes.exe")
                [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $staged, $true)
            } finally {
                $zip.Dispose()
            }

            Clear-DevlanesOldCopy $dir
            $inUse = Install-DevlanesFile -Staged $staged -Directory $dir -Name 'devlanes.exe'
        } finally {
            Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
        }

        Write-Host "Installed devlanes $version to $dir"
        if ($inUse) {
            Write-Host "The previous devlanes.exe is still running, probably as the daemon, and keeps running as $inUse."
            Write-Host 'Run `devlanes daemon restart` when convenient (it stops services); `devlanes doctor` then deletes the old file.'
        }

        if (Add-DevlanesUserPath $dir) {
            Write-Host "Added $dir to your user PATH; terminals opened from now on find devlanes."
        }
        if (-not (Test-DevlanesPathEntry $env:Path $dir)) {
            $env:Path = "$dir;$env:Path"
        }
        # New terminals get the machine PATH followed by the user PATH, so a devlanes in
        # the machine PATH (or before this directory in the user PATH) runs instead.
        $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $shadow = Find-DevlanesShadow "$machinePath;$userPath" $dir
        if ($shadow) {
            Write-Host "Another devlanes comes first on PATH: $shadow"
        }
    }

    # Windows PowerShell 5.1 may not offer TLS 1.2, which GitHub requires, when the
    # .NET defaults were changed; the process-wide setting is restored afterwards.
    $protocols = [Net.ServicePointManager]::SecurityProtocol
    try {
        if ($PSVersionTable.PSVersion.Major -lt 6 -and $protocols -ne [Net.SecurityProtocolType]::SystemDefault) {
            [Net.ServicePointManager]::SecurityProtocol = $protocols -bor [Net.SecurityProtocolType]::Tls12
        }
        Install-DevlanesRelease -Channel $Channel -Version $Version -Dir $Dir
    } finally {
        [Net.ServicePointManager]::SecurityProtocol = $protocols
    }
} @args

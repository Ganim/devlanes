<#
.SYNOPSIS
Packs the Windows release binaries for one target into OUT_DIR\devlanes-VERSION-TARGET.zip.

.DESCRIPTION
The archive holds one directory of the same name with devlanes.exe, devlanesw.exe,
devlanes-hosts-service.exe and LICENSE, like the .tar.gz archives package.sh writes
for macOS and Linux. Entries have a fixed order, a fixed time (SOURCE_DATE_EPOCH,
else the last commit's time) and no host-dependent attributes, so the same inputs
give the same bytes with the same PowerShell. Runs on Windows PowerShell 5.1 and
PowerShell 7 on any OS; check.sh runs it on Linux against fake binaries.

.EXAMPLE
pwsh packaging/scripts/package.ps1 -Version 0.1.0 -Target x86_64-pc-windows-msvc -OutDir dist
#>
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$OutDir,
    # Default: target\TARGET\release in the repository.
    [string]$BinDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0

if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$') {
    throw "package.ps1: not a release version: $Version"
}
if ($Target -notmatch '^(x86_64|aarch64)-pc-windows-msvc$') {
    throw "package.ps1: not a Windows release target: $Target"
}

$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BinDir) {
    $BinDir = Join-Path $root "target/$Target/release"
}
$name = "devlanes-$Version-$Target"
# Ordinal order, as `tar --sort=name` gives the Unix archives.
$files = @(
    @{ Entry = 'LICENSE'; Source = Join-Path $root 'LICENSE' },
    @{ Entry = 'devlanes-hosts-service.exe'; Source = Join-Path $BinDir 'devlanes-hosts-service.exe' },
    @{ Entry = 'devlanes.exe'; Source = Join-Path $BinDir 'devlanes.exe' },
    @{ Entry = 'devlanesw.exe'; Source = Join-Path $BinDir 'devlanesw.exe' }
)
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file.Source -PathType Leaf)) {
        throw "package.ps1: missing $($file.Source)"
    }
}

$epoch = $env:SOURCE_DATE_EPOCH
if (-not $epoch) {
    $epoch = & git -C $root log -1 --format=%ct 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $epoch) { $epoch = '0' }
}
# Zip stores local DOS times from 1980 to 2107 with two-second steps; .NET writes the
# clock time of the offset it is given, so a UTC offset keeps the host's zone out.
$seconds = [Math]::Min([Math]::Max([int64]$epoch, 315532800), 4354819198)
$stamp = [DateTimeOffset]::FromUnixTimeSeconds($seconds - ($seconds % 2))

Add-Type -AssemblyName System.IO.Compression
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$out = Join-Path (Resolve-Path -LiteralPath $OutDir).Path "$name.zip"
$partial = "$out.partial"
# ZipArchive leaves a Unix host's entries without permission bits, which unzip
# would extract as mode 0000; regular files get 0644 there. Windows ignores them.
$unixHost = [Environment]::OSVersion.Platform -eq [PlatformID]::Unix
$stream = [IO.File]::Open($partial, [IO.FileMode]::Create, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
try {
    $zip = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in $files) {
            $entry = $zip.CreateEntry("$name/$($file.Entry)", [IO.Compression.CompressionLevel]::Optimal)
            $entry.LastWriteTime = $stamp
            if ($unixHost) { $entry.ExternalAttributes = 0x81A4 -shl 16 }
            $writer = $entry.Open()
            $reader = [IO.File]::OpenRead($file.Source)
            try {
                $reader.CopyTo($writer)
            } finally {
                $reader.Dispose()
                $writer.Dispose()
            }
        }
    } finally {
        $zip.Dispose()
    }
} finally {
    $stream.Dispose()
}
Move-Item -LiteralPath $partial -Destination $out -Force
$out

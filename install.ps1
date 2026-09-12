#Requires -Version 5.1
<#
.SYNOPSIS
Install the mono binary from a GitHub release.

.DESCRIPTION
Downloads the Windows archive for the release selected by the pinned asset or
-Version, verifies it against the release's published SHA256SUMS,
and copies mono.exe to <Prefix>\bin. Mono keeps no state of its own, so undoing
this is Remove-Item on the installed path.

.EXAMPLE
irm https://github.com/Tomperez98/mono/releases/latest/download/install.ps1 | iex

.EXAMPLE
$env:MONO_VERSION = 'v0.1.5'
irm https://github.com/Tomperez98/mono/releases/latest/download/install.ps1 | iex
#>
[CmdletBinding()]
param(
    # Release to install, for example v0.1.5 or 0.1.5. Default: the release this
    # script was published with when it carries one, otherwise the newest, or
    # $env:MONO_VERSION when that is set. The environment variable is how the
    # release notes pin a version through a pipe, since `iex` cannot pass
    # -Version.
    [string]$Version = "",

    # Directory that receives bin\mono.exe. Default: $HOME\.local or
    # $env:MONO_INSTALL_DIR when set.
    [string]$Prefix = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repository = "Tomperez98/mono"
$Binary = "mono.exe"
$UserAgent = "mono-install"

#region published defaults
# Published release assets pin this region to a release and its archive
# digests. The checked-in source stays unpinned and requires -Version or
# MONO_VERSION.
$DefaultVersion = ""
$DefaultChecksums = ""
#endregion

function Get-BakedChecksum([string]$Checksums, [string]$Target) {
    if (-not $Checksums) { return "" }
    foreach ($Entry in $Checksums.Split(" ")) {
        if ($Entry.StartsWith("$Target=")) {
            return $Entry.Substring($Target.Length + 1)
        }
    }
    return ""
}

if (-not $Version -and $env:MONO_VERSION) {
    $Version = $env:MONO_VERSION
}
if (-not $Version -and $DefaultVersion) {
    $Version = $DefaultVersion
}

function Write-Note([string]$Message) {
    [Console]::Error.WriteLine("install.ps1: $Message")
}

function Fail([string]$Message) {
    [Console]::Error.WriteLine("install.ps1: $Message")
    exit 1
}

switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { $Target = "x86_64-pc-windows-msvc" }
    "ARM64" { Fail "no prebuilt mono for Windows ARM64; build from source with cargo install --path ." }
    default { Fail "no prebuilt mono for Windows architecture $($env:PROCESSOR_ARCHITECTURE)" }
}

if (-not $Prefix -and $env:MONO_INSTALL_DIR) {
    $Prefix = $env:MONO_INSTALL_DIR
}
if (-not $Prefix) {
    $Prefix = Join-Path $HOME ".local"
}

if (-not $Version) {
    Fail "this installer is not pinned; use a release asset or pass -Version"
}

# Accept both `0.1.5` and `v0.1.5`; the release tag carries the prefix.
$Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }

$Archive = "mono-$Tag-$Target.zip"
$Base = "https://github.com/$Repository/releases/download/$Tag"
$Temp = Join-Path ([System.IO.Path]::GetTempPath()) "mono-install-$([System.Guid]::NewGuid().ToString("N"))"
New-Item -ItemType Directory -Path $Temp | Out-Null

try {
    Write-Note "downloading $Archive"
    try {
        Invoke-WebRequest -UserAgent $UserAgent -Uri "$Base/$Archive" -OutFile (Join-Path $Temp $Archive)
    }
    catch {
        Fail "could not download $Base/$Archive"
    }

    # A published copy already carries this release's digest, so it needs neither
    # the API nor a second download. Any other release fetches SHA256SUMS.
    $Expected = ""
    if ($DefaultVersion -and $Tag -eq $DefaultVersion) {
        $Expected = Get-BakedChecksum -Checksums $DefaultChecksums -Target $Target
    }
    if (-not $Expected) {
        try {
            Invoke-WebRequest -UserAgent $UserAgent -Uri "$Base/SHA256SUMS" -OutFile (Join-Path $Temp "SHA256SUMS")
        }
        catch {
            Fail "could not download $Base/SHA256SUMS"
        }
        $Expected = Get-Content (Join-Path $Temp "SHA256SUMS") |
            Where-Object { ($_ -split '\s+')[1] -eq $Archive } |
            Select-Object -First 1
        if ($Expected) {
            $Expected = ($Expected -split '\s+')[0]
        }
    }
    if (-not $Expected) {
        Fail "SHA256SUMS has no entry for $Archive"
    }
    $Expected = $Expected.ToLowerInvariant()
    $Actual = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $Temp $Archive)).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Fail "checksum mismatch for ${Archive}: expected $Expected, got $Actual"
    }

    $Extracted = Join-Path $Temp "extracted"
    Expand-Archive -Path (Join-Path $Temp $Archive) -DestinationPath $Extracted -Force

    $BinDirectory = Join-Path $Prefix "bin"
    New-Item -ItemType Directory -Force -Path $BinDirectory | Out-Null
    $Installed = Join-Path $BinDirectory $Binary
    Copy-Item -Path (Join-Path $Extracted $Binary) -Destination $Installed -Force

    $UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($UserPath -notlike "*$BinDirectory*") {
        Write-Note "$BinDirectory is not on your user PATH; add it to run mono from anywhere"
    }

    # A version report is a smoke check, not a gate: the copy already succeeded.
    $Reported = $null
    try { $Reported = & $Installed --version } catch { }
    if ($Reported) {
        Write-Note "installed $Reported to $Installed"
    }
    else {
        Write-Note "installed $Installed"
    }
    Write-Note "remove it with: Remove-Item -Force '$Installed'"
}
finally {
    Remove-Item -Recurse -Force -Path $Temp -ErrorAction SilentlyContinue
}

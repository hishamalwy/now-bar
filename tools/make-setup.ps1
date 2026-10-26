# Builds what a GitHub release carries, into dist\: nowbar-setup.exe (both exes inside),
# nowbar.exe and nowbar-settings.exe (what the updater downloads), SHA256SUMS.txt and, with an
# update key (tools\make-update-key.ps1), its signature SHA256SUMS.txt.sig.
#
#   powershell -ExecutionPolicy Bypass -File tools\make-setup.ps1 [-Jobs 2] [-Clean]
#
# The C runtime is linked statically: the exes open on a Windows without the Visual C++
# redistributable. The dist profile is release with thin LTO and stripped symbols. Its build
# (a few GB, used only for a release) is kept: the next release rebuilds only what changed
# instead of every dependency. -Clean deletes it at the end. No exe may name the user who built it:
# the source paths in panic messages are remapped, and a path left over stops the release.
param([int]$Jobs = 2, [switch]$Clean)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Push-Location $repo
try {
    $meta = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
    $version = ($meta.packages | Where-Object name -eq 'nowbar').version
    $built = Join-Path $meta.target_directory 'dist'
    $dist = Join-Path $repo 'dist'
    $payload = Join-Path $dist 'payload'

    # Panic messages carry every crate's source path: the registry's under the cargo home, the
    # build scripts' output under this folder, both in the user's profile. Encoded flags: a path
    # may hold spaces.
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    $env:CARGO_ENCODED_RUSTFLAGS = @('-C', 'target-feature=+crt-static', "--remap-path-prefix=$cargoHome=cargo", "--remap-path-prefix=$repo=.") -join [char]0x1f
    cargo build -j $Jobs --profile dist --bin nowbar --bin nowbar-settings
    if ($LASTEXITCODE) { throw 'the exes did not build' }
    New-Item -ItemType Directory -Force $payload | Out-Null
    foreach ($exe in 'nowbar.exe', 'nowbar-settings.exe') { Copy-Item (Join-Path $built $exe) $payload -Force }

    $env:NOWBAR_SETUP_PAYLOAD = $payload
    $env:NOWBAR_VERSION = $version
    cargo build -j $Jobs --profile dist -p nowbar-setup
    if ($LASTEXITCODE) { throw 'the setup did not build' }
    Copy-Item (Join-Path $built 'nowbar-setup.exe') $dist -Force
    foreach ($exe in 'nowbar.exe', 'nowbar-settings.exe') { Copy-Item (Join-Path $payload $exe) $dist -Force }

    # The updater's format: "<sha256>  <name>" per line.
    $sums = foreach ($exe in 'nowbar-setup.exe', 'nowbar.exe', 'nowbar-settings.exe') {
        '{0}  {1}' -f (Get-FileHash (Join-Path $dist $exe) -Algorithm SHA256).Hash.ToLower(), $exe
    }
    Set-Content (Join-Path $dist 'SHA256SUMS.txt') $sums -Encoding ascii
    # Signed with the update key when this build has one (UPDATE_KEY): the updater swaps in only
    # what that key signed. Without one, users install updates with the setup.
    if ((Select-String -Path (Join-Path $repo 'src\update.rs') -Pattern 'pub const UPDATE_KEY: &str = "(.+)";').Matches.Count) {
        & (Join-Path $PSScriptRoot 'sign-sums.ps1') -Sums (Join-Path $dist 'SHA256SUMS.txt')
    }
    else { Write-Warning 'UPDATE_KEY in src\update.rs is empty: SHA256SUMS.txt is unsigned, and this build opens the release page instead of updating itself.' }

    # No exe may need the Visual C++ runtime DLLs, or name the user's folder.
    $userPath = '(?i)' + [regex]::Escape($env:USERPROFILE) + '|[\\/]users[\\/]' + [regex]::Escape($env:USERNAME) + '[\\/]'
    foreach ($exe in 'nowbar-setup.exe', 'nowbar.exe', 'nowbar-settings.exe') {
        $bytes = [System.IO.File]::ReadAllBytes((Join-Path $dist $exe))
        $text = [System.Text.Encoding]::ASCII.GetString($bytes)
        if ($text -match '(?i)vcruntime140|msvcp140') { throw "$exe still imports the Visual C++ runtime" }
        if ([System.Text.Encoding]::UTF8.GetString($bytes) -match $userPath -or [System.Text.Encoding]::Unicode.GetString($bytes) -match $userPath) { throw "$exe still holds a path in $env:USERPROFILE" }
        '{0,-22} {1,6:N1} MB' -f $exe, ($bytes.Length / 1MB)
    }
    "v$version -> $dist"
    Remove-Item $payload -Recurse -Force
    if ($Clean) {
        Remove-Item $built -Recurse -Force -ErrorAction SilentlyContinue
        'the dist build was deleted'
    }
}
finally {
    Pop-Location
}

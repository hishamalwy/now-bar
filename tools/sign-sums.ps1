# Signs a release's SHA256SUMS.txt with the update key (tools\make-update-key.ps1): writes
# SHA256SUMS.txt.sig beside it, the ECDSA P-256 signature (r and s, 64 bytes) of the file's
# SHA-256 as hex. The key must be the one UPDATE_KEY in src\update.rs names, or the release's
# users would refuse the update.
#
#   powershell -File tools\sign-sums.ps1 -Sums dist\SHA256SUMS.txt [-Key <file>]
param(
    [Parameter(Mandatory)][string]$Sums,
    [string]$Key = $(if ($env:NOWBAR_UPDATE_KEY) { $env:NOWBAR_UPDATE_KEY } else { Join-Path $env:USERPROFILE '.nowbar\update-key.json' })
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$public = (Select-String -Path (Join-Path $root 'src\update.rs') -Pattern 'pub const UPDATE_KEY: &str = "(.*)";').Matches[0].Groups[1].Value
if (-not $public) { throw 'UPDATE_KEY in src\update.rs is empty: make a key with tools\make-update-key.ps1 and put its public half there.' }
if (-not (Test-Path $Key)) { throw "No update key at $Key (set NOWBAR_UPDATE_KEY or pass -Key)." }
$k = Get-Content $Key -Raw | ConvertFrom-Json
if (($k.x + $k.y) -ne $public) { throw "$Key isn't the key UPDATE_KEY names: its users would refuse this release." }

$bytes = { param([string]$h) [byte[]]($h -split '(..)' -ne '' | ForEach-Object { [Convert]::ToByte($_, 16) }) }
$q = New-Object System.Security.Cryptography.ECPoint
$q.X = & $bytes $k.x
$q.Y = & $bytes $k.y
$p = New-Object System.Security.Cryptography.ECParameters
$p.Curve = [System.Security.Cryptography.ECCurve+NamedCurves]::nistP256
$p.Q = $q
$p.D = & $bytes $k.d
$ec = [System.Security.Cryptography.ECDsa]::Create($p)
# .NET signs in the IEEE P1363 form (r then s), the one Windows' BCrypt verifies.
$sig = $ec.SignData([IO.File]::ReadAllBytes((Resolve-Path $Sums)), [System.Security.Cryptography.HashAlgorithmName]::SHA256)
if (-not $ec.VerifyData([IO.File]::ReadAllBytes((Resolve-Path $Sums)), $sig, [System.Security.Cryptography.HashAlgorithmName]::SHA256)) { throw 'the signature does not verify' }
[IO.File]::WriteAllText("$Sums.sig", (-join ($sig | ForEach-Object { $_.ToString('x2') })) + "`n")
"signed: $Sums.sig"

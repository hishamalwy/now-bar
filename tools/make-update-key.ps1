# Makes the key that signs releases' SHA256SUMS.txt (ECDSA P-256): the updater installs only what
# this key signed. The private half goes to a file outside the repository, never into it; the
# public half is printed for UPDATE_KEY in src\update.rs. Once releases are signed, losing the
# private key means users must install the next version by hand.
#
#   powershell -ExecutionPolicy Bypass -File tools\make-update-key.ps1 [-Out <file>]
param([string]$Out = $(if ($env:NOWBAR_UPDATE_KEY) { $env:NOWBAR_UPDATE_KEY } else { Join-Path $env:USERPROFILE '.nowbar\update-key.json' }))
$ErrorActionPreference = 'Stop'
if (Test-Path $Out) { throw "$Out exists: a new key would make the releases it signed unverifiable. Move it away first." }
$hex = { param([byte[]]$b) -join ($b | ForEach-Object { $_.ToString('x2') }) }
$ec = [System.Security.Cryptography.ECDsa]::Create([System.Security.Cryptography.ECCurve+NamedCurves]::nistP256)
$p = $ec.ExportParameters($true)
New-Item -ItemType Directory -Force (Split-Path $Out -Parent) | Out-Null
$json = @{ curve = 'P-256'; d = (& $hex $p.D); x = (& $hex $p.Q.X); y = (& $hex $p.Q.Y) } | ConvertTo-Json
[IO.File]::WriteAllText($Out, $json)
# Only this user may read it.
icacls $Out /inheritance:r /grant:r "${env:USERNAME}:(R,W)" | Out-Null
"Private key: $Out (keep a backup somewhere safe; never commit it)"
"Public key for UPDATE_KEY in src\update.rs:"
(& $hex $p.Q.X) + (& $hex $p.Q.Y)

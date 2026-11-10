# dist\hitori-<version>-windows-x86_64.zip: hitori.exe with xray.exe next to it.
# Needs target\release\hitori.exe and bin\xray-windows-amd64.exe (scripts/fetch-xray.sh).
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
if (-not $env:VERSION) { throw "set VERSION" }
$name = "hitori-$($env:VERSION)-windows-x86_64"
$stage = Join-Path $root "dist\stage\$name"

if (Test-Path (Join-Path $root "dist\stage")) { Remove-Item -Recurse -Force (Join-Path $root "dist\stage") }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item (Join-Path $root "target\release\hitori.exe") (Join-Path $stage "hitori.exe")
Copy-Item (Join-Path $root "bin\xray-windows-amd64.exe") (Join-Path $stage "xray.exe")

$zip = Join-Path $root "dist\$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip
Remove-Item -Recurse -Force (Join-Path $root "dist\stage")
Write-Output "package-windows: dist\$name.zip"

$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$installer = Join-Path $scriptDir "install.ps1"
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
$fixtures = Join-Path $testRoot "fixtures"
$mockBin = Join-Path $testRoot "bin"
New-Item -ItemType Directory -Force -Path $fixtures, $mockBin | Out-Null

try {
  Copy-Item /bin/echo (Join-Path $fixtures "cgx.exe")
  Set-Content -Path (Join-Path $fixtures "cgx-x86_64-pc-windows-msvc.zip") -Value "verified archive"
  $hash = (Get-FileHash (Join-Path $fixtures "cgx-x86_64-pc-windows-msvc.zip") -Algorithm SHA256).Hash.ToLowerInvariant()
  Set-Content -Path (Join-Path $fixtures "cgx-x86_64-pc-windows-msvc.zip.sha256") -Value "$hash  cgx-x86_64-pc-windows-msvc.zip"

  $distInstaller = @'
$dest = Join-Path $env:CGX_INSTALL_DIR "bin"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item $env:MOCK_CGX (Join-Path $dest "cgx.exe") -Force
& chmod +x (Join-Path $dest "cgx.exe")
'@
  Set-Content -Path (Join-Path $fixtures "cgx-installer.ps1") -Value $distInstaller

  $gh = @'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$GH_LOG"
case "$1 $2 ${3:-}" in
  "release verify-asset --help" | "attestation verify --help") exit 0 ;;
  "release view "*)
    if [ "${GH_RELEASE_MUTABLE:-false}" = true ]; then
      printf '{"tagName":"v0.1.0","isDraft":false,"isImmutable":false}\n'
    else
      printf '{"tagName":"v0.1.0","isDraft":false,"isImmutable":true}\n'
    fi
    ;;
  "release verify-asset "*) exit 0 ;;
  "attestation verify "*) [ "${GH_FAIL_ATTESTATION:-false}" != true ] ;;
  *) exit 64 ;;
esac
'@
  Set-Content -Path (Join-Path $mockBin "gh") -Value $gh
  & chmod +x (Join-Path $mockBin "gh")

  $cargo = @'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$CARGO_LOG"
exit 99
'@
  Set-Content -Path (Join-Path $mockBin "cargo") -Value $cargo
  & chmod +x (Join-Path $mockBin "cargo")

  $sleep = @'
#!/usr/bin/env sh
exit 0
'@
  Set-Content -Path (Join-Path $mockBin "sleep") -Value $sleep
  & chmod +x (Join-Path $mockBin "sleep")

  $wrapper = @'
function Invoke-WebRequest {
  param(
    [Parameter(Position = 0)] [string] $Uri,
    [string] $OutFile,
    $Headers,
    [int] $MaximumRetryCount,
    [int] $RetryIntervalSec
  )
  $asset = Split-Path -Leaf $Uri
  Add-Content -Path $env:DOWNLOAD_LOG -Value $Uri
  Copy-Item (Join-Path $env:FIXTURES $asset) $OutFile -Force
}

function Invoke-RestMethod {
  param(
    [Parameter(Position = 0)] [string] $Uri,
    [int] $MaximumRetryCount,
    [int] $RetryIntervalSec
  )
  Add-Content -Path $env:DOWNLOAD_LOG -Value $Uri
  Get-Content (Join-Path $env:FIXTURES (Split-Path -Leaf $Uri)) -Raw
}

function Expand-Archive {
  param([string] $Path, [string] $DestinationPath, [switch] $Force)
  New-Item -ItemType Directory -Force -Path $DestinationPath | Out-Null
  Copy-Item (Join-Path $env:FIXTURES "cgx.exe") (Join-Path $DestinationPath "cgx.exe") -Force
  & chmod +x (Join-Path $DestinationPath "cgx.exe")
}

. $env:INSTALLER
'@
  $wrapperPath = Join-Path $testRoot "wrapper.ps1"
  Set-Content -Path $wrapperPath -Value $wrapper

  function Invoke-TestCase($name, $verify, $target = "", $mutable = "false", $failAttestation = "false") {
    $caseDir = Join-Path $testRoot $name
    New-Item -ItemType Directory -Force -Path $caseDir | Out-Null
    $env:PATH = "$mockBin$([IO.Path]::PathSeparator)$script:originalPath"
    $env:INSTALLER = $installer
    $env:FIXTURES = $fixtures
    $env:MOCK_CGX = Join-Path $fixtures "cgx.exe"
    $env:DOWNLOAD_LOG = Join-Path $caseDir "download.log"
    $env:GH_LOG = Join-Path $caseDir "gh.log"
    $env:CARGO_LOG = Join-Path $caseDir "cargo.log"
    $env:CARGO_HOME = Join-Path $caseDir "cargo-home"
    $env:GITHUB_OUTPUT = Join-Path $caseDir "output"
    $env:GITHUB_PATH = Join-Path $caseDir "path"
    $env:RUNNER_ARCH = "X64"
    $env:INPUT_VERSION = "latest"
    $env:INPUT_TARGET = $target
    $env:INPUT_CARGO_CGX = "false"
    $env:INPUT_VERIFY_ATTESTATIONS = $verify
    $env:GH_RELEASE_MUTABLE = $mutable
    $env:GH_FAIL_ATTESTATION = $failAttestation
    New-Item -ItemType File -Force -Path $env:DOWNLOAD_LOG, $env:GH_LOG, $env:CARGO_LOG, $env:GITHUB_OUTPUT, $env:GITHUB_PATH | Out-Null
    $output = & pwsh -NoProfile -File $wrapperPath 2>&1
    $exitCode = $LASTEXITCODE
    return [pscustomobject]@{
      ExitCode = $exitCode
      Output = ($output -join "`n")
      Directory = $caseDir
    }
  }

  $originalPath = $env:PATH

  $verified = Invoke-TestCase "verified" "true"
  if ($verified.ExitCode -ne 0) { throw $verified.Output }
  if (-not (Test-Path (Join-Path $verified.Directory "cargo-home/bin/cgx.exe"))) { throw "verified install did not install cgx" }
  $ghLog = Get-Content (Join-Path $verified.Directory "gh.log") -Raw
  if ($ghLog -notmatch 'release verify-asset v0.1.0') { throw "release asset was not verified" }
  if ($ghLog -notmatch 'signer-workflow anelson/cgx/.github/workflows/release.yml') { throw "signer workflow was not enforced" }
  if ($ghLog -notmatch 'source-ref refs/tags/v0.1.0') { throw "source ref was not enforced" }

  $unverified = Invoke-TestCase "unverified" "false"
  if ($unverified.ExitCode -ne 0) { throw $unverified.Output }
  if ((Get-Item (Join-Path $unverified.Directory "gh.log")).Length -ne 0) { throw "unverified install invoked gh" }
  if ((Get-Content (Join-Path $unverified.Directory "download.log") -Raw) -notmatch 'cgx-installer.ps1') { throw "unverified native install did not use cargo-dist" }

  $explicit = Invoke-TestCase "explicit" "false" "x86_64-pc-windows-msvc"
  if ($explicit.ExitCode -ne 0) { throw $explicit.Output }
  if ((Get-Item (Join-Path $explicit.Directory "gh.log")).Length -ne 0) { throw "unverified explicit install invoked gh" }
  if ((Get-Content (Join-Path $explicit.Directory "download.log") -Raw) -match 'installer.ps1') { throw "explicit install used cargo-dist installer" }

  $mutable = Invoke-TestCase "mutable" "true" "" "true"
  if ($mutable.ExitCode -eq 0) { throw "mutable release was accepted: $($mutable.Output)" }
  if ((Get-Item (Join-Path $mutable.Directory "cargo.log")).Length -ne 0) { throw "mutable release used source fallback" }

  $badAttestation = Invoke-TestCase "bad-attestation" "true" "" "false" "true"
  if ($badAttestation.ExitCode -eq 0) { throw "invalid attestation was accepted: $($badAttestation.Output)" }
  if ((Get-Item (Join-Path $badAttestation.Directory "cargo.log")).Length -ne 0) { throw "attestation failure used source fallback" }

  Write-Output "cgx action PowerShell installer tests passed"
} finally {
  if ($originalPath) { $env:PATH = $originalPath }
  Remove-Item -Recurse -Force $testRoot -ErrorAction SilentlyContinue
}

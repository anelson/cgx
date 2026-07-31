# Install cgx (and optionally cargo-cgx) on a Windows runner for the
# `anelson/cgx` GitHub Action. Prefers prebuilt binaries; falls back to
# `cargo install`.
#
# Inputs (environment):
#   INPUT_VERSION              "latest" or "vX.Y.Z" (normalized by action.yml)
#   INPUT_TARGET               target triple to force, or empty for native detection
#   INPUT_CARGO_CGX            "true" to also install cargo-cgx
#   INPUT_VERIFY_ATTESTATIONS  "true" to require release and artifact attestations
#   CGX_GITHUB_TOKEN           token for authenticated GitHub requests
#
# Outputs (written to $env:GITHUB_OUTPUT): version, cgx-version, path

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false

class SecurityVerificationException : System.Exception {
  SecurityVerificationException([string]$message) : base($message) {}
}

class PrebuiltUnavailableException : System.Exception {
  PrebuiltUnavailableException([string]$message) : base($message) {}
}

$repo = "anelson/cgx"
$signerWorkflow = "anelson/cgx/.github/workflows/release.yml"
$version = if ($env:INPUT_VERSION) { $env:INPUT_VERSION } else { "latest" }
$requestedTarget = $env:INPUT_TARGET
$wantCargoCgx = ($env:INPUT_CARGO_CGX -eq "true")
$verifyAttestationsInput = if ($env:INPUT_VERIFY_ATTESTATIONS) { $env:INPUT_VERIFY_ATTESTATIONS } else { "true" }

if ($verifyAttestationsInput -notin @("true", "false")) {
  Write-Output "::error::cgx: verify-attestations must be 'true' or 'false'"
  exit 1
}
$verifyAttestations = ($verifyAttestationsInput -eq "true")

$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
$dest = Join-Path $cargoHome "bin"
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$base = if ($version -eq "latest") {
  "https://github.com/$repo/releases/latest/download"
} else {
  "https://github.com/$repo/releases/download/$version"
}
$resolvedTag = $null

function Get-DownloadHeaders {
  $headers = @{}
  if ($env:CGX_GITHUB_TOKEN) { $headers["Authorization"] = "Bearer $($env:CGX_GITHUB_TOKEN)" }
  return $headers
}

function Invoke-DownloadFile($url, $path) {
  try {
    $null = Invoke-WebRequest $url -OutFile $path -Headers (Get-DownloadHeaders) -MaximumRetryCount 5 -RetryIntervalSec 2
  } catch {
    throw [PrebuiltUnavailableException]::new("failed to download $url")
  }
}

function Test-Sha256($archive, $sidecar) {
  $expected = (((Get-Content $sidecar -Raw) -split '\s+')[0]).ToLowerInvariant()
  if ($expected -notmatch '^[0-9a-f]{64}$') { return $false }
  $actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
  return ($expected -eq $actual)
}

function Invoke-GhVerification($description, [string[]]$arguments) {
  for ($attempt = 1; $attempt -le 3; $attempt++) {
    & gh @arguments
    if ($LASTEXITCODE -eq 0) { return }
    if ($attempt -lt 3) { Start-Sleep -Seconds ($attempt * 2) }
  }
  throw [SecurityVerificationException]::new($description)
}

function Resolve-VerifiedRelease {
  if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw [SecurityVerificationException]::new("GitHub CLI is required when verify-attestations is enabled")
  }
  & gh release verify-asset --help *> $null
  if ($LASTEXITCODE -ne 0) {
    throw [SecurityVerificationException]::new("GitHub CLI does not support immutable-release verification")
  }
  & gh attestation verify --help *> $null
  if ($LASTEXITCODE -ne 0) {
    throw [SecurityVerificationException]::new("GitHub CLI does not support artifact-attestation verification")
  }

  if ($env:CGX_GITHUB_TOKEN) { $env:GH_TOKEN = $env:CGX_GITHUB_TOKEN }
  $arguments = @("release", "view")
  if ($version -ne "latest") { $arguments += $version }
  $arguments += @("--repo", $repo, "--json", "tagName,isDraft,isImmutable")

  $json = $null
  for ($attempt = 1; $attempt -le 3; $attempt++) {
    $json = & gh @arguments
    if ($LASTEXITCODE -eq 0) { break }
    if ($attempt -lt 3) { Start-Sleep -Seconds ($attempt * 2) }
  }
  if ($LASTEXITCODE -ne 0) {
    throw [SecurityVerificationException]::new("failed to resolve GitHub release '$version'")
  }

  $release = $json | ConvertFrom-Json
  if (-not $release.tagName -or $release.isDraft -or -not $release.isImmutable) {
    throw [SecurityVerificationException]::new("release '$version' is not a published immutable release")
  }
  return $release.tagName
}

function Resolve-NativeTarget {
  switch ($env:RUNNER_ARCH) {
    "X64" { return "x86_64-pc-windows-msvc" }
    "ARM64" { return "aarch64-pc-windows-msvc" }
    default { return $null }
  }
}

function Test-WindowsReleaseTarget($target) {
  return $target -in @("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")
}

function Install-VerifiedArchives($releaseTag, $target) {
  $names = @("cgx")
  if ($wantCargoCgx) { $names += "cargo-cgx" }
  $work = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
  New-Item -ItemType Directory -Force -Path $work | Out-Null
  try {
    foreach ($name in $names) {
      $archiveName = "$name-$target.zip"
      $archive = Join-Path $work $archiveName
      $sidecar = "$archive.sha256"
      Invoke-DownloadFile "$base/$archiveName" $archive
      Invoke-DownloadFile "$base/$archiveName.sha256" $sidecar
      if (-not (Test-Sha256 $archive $sidecar)) {
        throw [SecurityVerificationException]::new("SHA-256 verification failed for $archiveName")
      }
      $releaseVerification = @("release", "verify-asset", $releaseTag, $archive, "--repo", $repo)
      Invoke-GhVerification "immutable-release verification failed for $archiveName" $releaseVerification
      $provenanceVerification = @(
        "attestation", "verify", $archive,
        "--repo", $repo,
        "--signer-workflow", $signerWorkflow,
        "--source-ref", "refs/tags/$releaseTag"
      )
      Invoke-GhVerification "build-provenance verification failed for $archiveName" $provenanceVerification
    }

    foreach ($name in $names) {
      $archiveName = "$name-$target.zip"
      $extract = Join-Path $work "extract-$name"
      Expand-Archive -Path (Join-Path $work $archiveName) -DestinationPath $extract -Force
      Copy-Item (Join-Path $extract "$name.exe") (Join-Path $dest "$name.exe") -Force
    }
  } catch [SecurityVerificationException] {
    throw
  } catch [PrebuiltUnavailableException] {
    throw
  } catch {
    throw [PrebuiltUnavailableException]::new($_.Exception.Message)
  } finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
  }
}

function Invoke-DistInstaller($url) {
  # Run the installer in a child process so its `exit` cannot bypass fallback handling.
  $script = Invoke-RestMethod $url -MaximumRetryCount 5 -RetryIntervalSec 2
  $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("cgx-installer-" + [System.Guid]::NewGuid().ToString() + ".ps1")
  Set-Content -Path $tmp -Value $script -Encoding utf8
  try {
    & pwsh -NoProfile -ExecutionPolicy Bypass -File $tmp
    if ($LASTEXITCODE -ne 0) { throw "installer $url exited with code $LASTEXITCODE" }
  } finally {
    Remove-Item -Force $tmp -ErrorAction SilentlyContinue
  }
}

function Install-ViaDistInstaller {
  # The cargo-dist installer appends \bin to CGX_INSTALL_DIR itself.
  $env:CGX_INSTALL_DIR = $cargoHome
  $env:CGX_DISABLE_UPDATE = "1"
  $env:CGX_UNMANAGED_INSTALL = "1"
  Invoke-DistInstaller "$base/cgx-installer.ps1"
  if ($wantCargoCgx) { Invoke-DistInstaller "$base/cargo-cgx-installer.ps1" }
}

function Install-ViaUnverifiedManualDownload {
  $names = @("cgx")
  if ($wantCargoCgx) { $names += "cargo-cgx" }

  foreach ($name in $names) {
    $archiveName = "$name-$requestedTarget.zip"
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    try {
      $archive = Join-Path $work $archiveName
      $sidecar = "$archive.sha256"
      $null = Invoke-WebRequest "$base/$archiveName" -OutFile $archive -Headers (Get-DownloadHeaders) -MaximumRetryCount 5 -RetryIntervalSec 2
      $null = Invoke-WebRequest "$base/$archiveName.sha256" -OutFile $sidecar -Headers (Get-DownloadHeaders) -MaximumRetryCount 5 -RetryIntervalSec 2
      if (-not (Test-Sha256 $archive $sidecar)) { throw "sha256 mismatch for $archiveName" }
      Expand-Archive -Path $archive -DestinationPath $work -Force
      Copy-Item (Join-Path $work "$name.exe") (Join-Path $dest "$name.exe") -Force
    } finally {
      Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
  }
}

function Invoke-SourceFallback {
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "no prebuilt binary for this platform and no Rust toolchain (cargo) to build from source"
  }
  $selectedVersion = if ($resolvedTag) { $resolvedTag } else { $version }
  $v = if ($selectedVersion -ne "latest") { $selectedVersion.TrimStart("v") } else { $null }
  $targetArgs = if ($requestedTarget) { @("--target", $requestedTarget) } else { @() }

  if ($v) { cargo install --locked @targetArgs cgx --version $v } else { cargo install --locked @targetArgs cgx }
  if ($LASTEXITCODE -ne 0) { throw "cargo install cgx failed" }

  if ($wantCargoCgx) {
    if ($v) { cargo install --locked @targetArgs cargo-cgx --version $v } else { cargo install --locked @targetArgs cargo-cgx }
    if ($LASTEXITCODE -ne 0) { throw "cargo install cargo-cgx failed" }
  }
}

$useSourceFallback = $false
$fallbackReason = $null
if ($verifyAttestations) {
  try {
    $resolvedTag = Resolve-VerifiedRelease
    $base = "https://github.com/$repo/releases/download/$resolvedTag"
    $selectedTarget = if ($requestedTarget) { $requestedTarget } else { Resolve-NativeTarget }
    if (-not $selectedTarget -or -not (Test-WindowsReleaseTarget $selectedTarget)) {
      $useSourceFallback = $true
    } else {
      Install-VerifiedArchives $resolvedTag $selectedTarget
    }
  } catch [PrebuiltUnavailableException] {
    $useSourceFallback = $true
    $fallbackReason = $_.Exception.Message
  } catch [SecurityVerificationException] {
    Write-Output "::error::cgx: $($_.Exception.Message)"
    throw
  }
} else {
  try {
    if ([string]::IsNullOrEmpty($requestedTarget)) {
      Install-ViaDistInstaller
    } else {
      Install-ViaUnverifiedManualDownload
    }
  } catch {
    $useSourceFallback = $true
    $fallbackReason = $_.Exception.Message
  }
}

if ($useSourceFallback) {
  $reason = if ($fallbackReason) { " ($fallbackReason)" } else { "" }
  Write-Output "::warning::cgx: prebuilt install failed$reason; building from source with 'cargo install --locked'"
  Invoke-SourceFallback
}

if (($env:Path -split ';') -notcontains $dest) {
  if ($env:GITHUB_PATH) { Add-Content -Path $env:GITHUB_PATH -Value $dest }
}

$cgxBin = Join-Path $dest "cgx.exe"
if (-not (Test-Path $cgxBin)) {
  $found = Get-Command cgx -ErrorAction SilentlyContinue
  if ($found) { $cgxBin = $found.Source }
}
$cgxVersion = ""
if (Test-Path $cgxBin) {
  # Older releases print --version to stderr; the version is the second field.
  $line = (& $cgxBin --version 2>&1 | ForEach-Object { "$_" } | Select-Object -First 1)
  if ($line) { $cgxVersion = ($line.Trim() -split '\s+')[1] }
}

"version=$version" | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8
"cgx-version=$cgxVersion" | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8
"path=$cgxBin" | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8

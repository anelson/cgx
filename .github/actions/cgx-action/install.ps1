# Install cgx (and optionally cargo-cgx) on a Windows runner for the
# `anelson/cgx` GitHub Action. Prefers prebuilt binaries; falls back to
# `cargo install`.
#
# Inputs (environment):
#   INPUT_VERSION     "latest" or "vX.Y.Z" (already normalized by action.yml)
#   INPUT_TARGET      target triple to force, or empty for native auto-detect
#   INPUT_CARGO_CGX   "true" to also install cargo-cgx
#   CGX_GITHUB_TOKEN  token for authenticated downloads
#
# Outputs (written to $env:GITHUB_OUTPUT): version, cgx-version, path

$ErrorActionPreference = "Stop"

$repo = "anelson/cgx"
$version = if ($env:INPUT_VERSION) { $env:INPUT_VERSION } else { "latest" }
$target = $env:INPUT_TARGET
$wantCargoCgx = ($env:INPUT_CARGO_CGX -eq "true")

$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
$dest = Join-Path $cargoHome "bin"
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$base = if ($version -eq "latest") {
  "https://github.com/$repo/releases/latest/download"
} else {
  "https://github.com/$repo/releases/download/$version"
}

# Run a cargo-dist installer in a CHILD pwsh process. The installer ends with
# `exit 1` on failure; running it inline (Invoke-Expression) would terminate this
# process and bypass the source-build fallback in the caller's try/catch. A child
# process turns that `exit` into $LASTEXITCODE, which we surface as a throw. We
# write the script to a temp file (it is too large to pass via -Command) and run
# it with -ExecutionPolicy Bypass (one of the policies the installer accepts).
function Invoke-DistInstaller($url) {
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

# Default path: the cargo-dist installer auto-detects the platform, verifies
# checksums, installs to $cargoHome\bin, and adds it to $GITHUB_PATH. The
# installer appends "\bin" to CGX_INSTALL_DIR itself, so point it at the cargo
# home root (not $dest, which already ends in \bin) to avoid a nested bin\bin.
function Install-ViaDistInstaller {
  $env:CGX_INSTALL_DIR = $cargoHome
  $env:CGX_DISABLE_UPDATE = "1"
  $env:CGX_UNMANAGED_INSTALL = "1"
  Invoke-DistInstaller "$base/cgx-installer.ps1"
  if ($wantCargoCgx) {
    Invoke-DistInstaller "$base/cargo-cgx-installer.ps1"
  }
}

# Override path: fetch the exact archive, verify its .sha256 sidecar, extract.
# Windows archives are .zip with the binary at the zip root (no subdir).
function Install-ViaManualDownload {
  $headers = @{}
  if ($env:CGX_GITHUB_TOKEN) { $headers["Authorization"] = "Bearer $($env:CGX_GITHUB_TOKEN)" }

  $names = @("cgx")
  if ($wantCargoCgx) { $names += "cargo-cgx" }

  foreach ($name in $names) {
    $archive = "$name-$target.zip"
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    try {
      $zip = Join-Path $work $archive
      Invoke-WebRequest "$base/$archive" -OutFile $zip -Headers $headers -MaximumRetryCount 5 -RetryIntervalSec 2
      Invoke-WebRequest "$base/$archive.sha256" -OutFile "$zip.sha256" -Headers $headers -MaximumRetryCount 5 -RetryIntervalSec 2
      $want = (((Get-Content "$zip.sha256" -Raw) -split '\s+')[0]).ToLower()
      $got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
      if ($want -ne $got) { throw "sha256 mismatch for $archive" }
      Expand-Archive -Path $zip -DestinationPath $work -Force
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
  $v = if ($version -ne "latest") { $version.TrimStart("v") } else { $null }

  # Honor an explicitly requested target so the source build matches the request
  # instead of silently building a host-native binary.
  $targetArgs = if ($target) { @("--target", $target) } else { @() }

  if ($v) { cargo install --locked @targetArgs cgx --version $v } else { cargo install --locked @targetArgs cgx }
  if ($LASTEXITCODE -ne 0) { throw "cargo install cgx failed" }

  if ($wantCargoCgx) {
    if ($v) { cargo install --locked @targetArgs cargo-cgx --version $v } else { cargo install --locked @targetArgs cargo-cgx }
    if ($LASTEXITCODE -ne 0) { throw "cargo install cargo-cgx failed" }
  }
}

try {
  if ([string]::IsNullOrEmpty($target)) {
    Install-ViaDistInstaller
  } else {
    Install-ViaManualDownload
  }
} catch {
  Write-Output "::warning::cgx: prebuilt install failed ($($_.Exception.Message)); building from source with 'cargo install --locked'"
  Invoke-SourceFallback
}

# The cargo-dist installer adds $dest to $GITHUB_PATH; do it for the manual/source
# paths too (idempotent).
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
  $line = (& $cgxBin --version 2>$null | Select-Object -First 1)
  # `cgx --version` prints "cgx <version>" (optionally " (<sha> <date>)"), so the
  # version is the second whitespace-separated field, not the last.
  if ($line) { $cgxVersion = ($line.Trim() -split '\s+')[1] }
}

"version=$version"        | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8
"cgx-version=$cgxVersion" | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8
"path=$cgxBin"           | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8

$ErrorActionPreference = 'Stop'

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$targetTriple = $env:TAURI_ENV_TARGET_TRIPLE
if ([string]::IsNullOrWhiteSpace($targetTriple)) {
  $targetTriple = 'x86_64-pc-windows-msvc'
}
if ($targetTriple -notmatch '^x86_64-pc-windows-msvc$') {
  throw "RustBackup currently packages Windows x64 MSVC only; got '$targetTriple'."
}

if ([string]::IsNullOrWhiteSpace($env:RUSTBACKUP_GOOGLE_OAUTH_CLIENT_ID)) {
  $env:RUSTBACKUP_GOOGLE_OAUTH_CLIENT_ID = '647224236237-f4on9hiol9mf7jbnm365bbv0hvbqvbbf.apps.googleusercontent.com'
}

Push-Location $repositoryRoot
try {
  & cargo build --release --package rustbackup-agent --target $targetTriple
  if ($LASTEXITCODE -ne 0) {
    throw "Building rustbackup-agent failed with exit code $LASTEXITCODE."
  }

  $agentSource = Join-Path $repositoryRoot "target\$targetTriple\release\rustbackup-agent.exe"
  if (-not (Test-Path -LiteralPath $agentSource -PathType Leaf)) {
    throw "Built Agent binary was not found at '$agentSource'."
  }

  $binaryDirectory = Join-Path $repositoryRoot 'apps\desktop\binaries'
  New-Item -ItemType Directory -Path $binaryDirectory -Force | Out-Null
  $stagedAgent = Join-Path $binaryDirectory "rustbackup-agent-$targetTriple.exe"
  Copy-Item -LiteralPath $agentSource -Destination $stagedAgent -Force
  Write-Output "Staged Agent sidecar: $stagedAgent"
}
finally {
  Pop-Location
}

$ErrorActionPreference = 'Stop'

$repositoryRoot = Split-Path -Parent $PSScriptRoot
Push-Location (Join-Path $repositoryRoot 'apps\desktop')
try {
  & cargo tauri build --bundles nsis
  if ($LASTEXITCODE -ne 0) {
    throw "Tauri Windows installer build failed with exit code $LASTEXITCODE."
  }
}
finally {
  Pop-Location
}

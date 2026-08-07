param(
    [string]$Name = "1h-agent-windows-x86_64",
    [string]$OutputDir = "../1H-Agent-Release"
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$archive = Join-Path $OutputDir "$Name.zip"
Compress-Archive -Path target/release/1h-agent.exe -DestinationPath $archive -Force
$hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
Set-Content -NoNewline -Path "$archive.sha256" -Value "$hash  $Name.zip"

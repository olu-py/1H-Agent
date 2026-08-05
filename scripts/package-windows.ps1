param(
    [string]$Name = "1h-agent-windows-x86_64"
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path dist | Out-Null
$archive = "dist/$Name.zip"
Compress-Archive -Path target/release/1h-agent.exe -DestinationPath $archive -Force
$hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
Set-Content -NoNewline -Path "$archive.sha256" -Value "$hash  $Name.zip"


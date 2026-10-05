# Installiert Just Video aufs Steam Frame (über SSH, braucht den Entwicklermodus).
# Start (PowerShell, im Repo-Ordner):
#   powershell -ExecutionPolicy Bypass -File windows\deploy.ps1
# oder mit IP-Adresse, falls frame.local nicht gefunden wird:
#   powershell -ExecutionPolicy Bypass -File windows\deploy.ps1 -Frame 192.168.1.50
# Auf dem Frame vorher: Einstellungen > System > Entwicklermodus an,
# dann unter Entwickler ein Benutzerpasswort setzen. Nach diesem Passwort wird
# gefragt (zweimal: einmal zum Kopieren, einmal zum Installieren).
param([string]$Frame = "frame.local")
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$pkg = Join-Path $repo "out\JustVideo"

if (-not (Test-Path (Join-Path $pkg "just-video"))) {
    Write-Host "Noch kein Paket da. Erst bauen: windows\build.ps1" -ForegroundColor Red
    exit 1
}
if (-not (Get-Command ssh -ErrorAction SilentlyContinue)) {
    Write-Host "ssh fehlt. Windows-Einstellungen > Apps > Optionale Features > 'OpenSSH-Client' hinzufügen." -ForegroundColor Red
    exit 1
}

$target = "steamos@$Frame"
$opts = @("-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=15")

Write-Host "== Kopiere Just Video aufs Frame ($Frame)" -ForegroundColor Cyan
Write-Host "   Passwort = das Benutzerpasswort aus dem Entwicklermodus des Frames."
& scp @opts -r "$pkg" "${target}:.cache/"
if ($LASTEXITCODE -ne 0) {
    Write-Host "Kopieren fehlgeschlagen. Ist das Frame an, im gleichen Netzwerk und der Entwicklermodus aktiv?" -ForegroundColor Red
    Write-Host "Sonst mit IP versuchen: windows\deploy.ps1 -Frame <IP-Adresse des Frames>"
    exit 1
}

Write-Host "== Installiere auf dem Frame" -ForegroundColor Cyan
Write-Host "   Beim ersten Mal startet Steam auf dem Frame kurz neu (normal)."
& ssh @opts $target "bash ~/.cache/JustVideo/install-on-frame.sh"
if ($LASTEXITCODE -ne 0) {
    Write-Host "Installation fehlgeschlagen (siehe Meldungen oben)." -ForegroundColor Red
    exit 1
}
Write-Host ""
Write-Host "Fertig! Auf dem Frame: Steam > Bibliothek > Nicht-Steam > Just Video" -ForegroundColor Green

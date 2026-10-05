# Installs Just Video on the Steam Frame over SSH (Developer Mode must be on).
# Installiert Just Video über SSH aufs Steam Frame (Entwicklermodus muss an sein).
#
# Works in an unpacked release zip (next to just-video: right-click this file
# > Run with PowerShell) and in the repository after windows\build.ps1.
# Geht in einer entpackten Release-Zip (Rechtsklick > Mit PowerShell ausführen)
# und im Repo nach windows\build.ps1.
#
#   powershell -ExecutionPolicy Bypass -File deploy.ps1
#   powershell -ExecutionPolicy Bypass -File deploy.ps1 -Frame 192.168.1.50
#
# On the Frame first: Settings > System > Enable Developer Mode, then
# Developer > Set User Password. That password is asked for twice (copying,
# installing) unless an SSH key is set up (see README).
param(
    # The Frame's address: frame.local or its IP address.
    [string]$Frame = "frame.local",
    # Don't wait for Enter at the end (e.g. when started from a terminal).
    [switch]$NoPause
)
$ErrorActionPreference = "Stop"

function Finish([int]$code) {
    if (-not $NoPause) {
        Read-Host "Enter = close / schließen" | Out-Null
    }
    exit $code
}

# The package: this script's folder (release zip) or out\JustVideo (own build).
if (Test-Path (Join-Path $PSScriptRoot "just-video")) {
    $pkg = $PSScriptRoot
} else {
    $pkg = Join-Path (Split-Path -Parent $PSScriptRoot) "out\JustVideo"
}
if (-not (Test-Path (Join-Path $pkg "just-video"))) {
    Write-Host "No package found: build first (windows\build.ps1)." -ForegroundColor Red
    Write-Host "Kein Paket gefunden: erst bauen (windows\build.ps1)." -ForegroundColor Red
    Finish 1
}
if (-not (Get-Command ssh -ErrorAction SilentlyContinue)) {
    Write-Host "ssh is missing: Settings > Apps > Optional features > add 'OpenSSH Client'." -ForegroundColor Red
    Write-Host "ssh fehlt: Einstellungen > Apps > Optionale Features > 'OpenSSH-Client' hinzufügen." -ForegroundColor Red
    Finish 1
}

$target = "steamos@$Frame"
$opts = @("-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=15")
# A fresh folder on the headset each time; removed after installing.
$tmp = ".cache/jv-install-" + (Get-Date -Format "yyyyMMddHHmmss")

Write-Host "== Copying Just Video to $Frame / Kopiere Just Video auf $Frame" -ForegroundColor Cyan
Write-Host "   Password = the Frame's Developer Mode user password / Passwort = Benutzerpasswort aus dem Entwicklermodus"
& scp @opts -r "$pkg" "${target}:$tmp"
if ($LASTEXITCODE -ne 0) {
    Write-Host "Copying failed. Is the Frame on, awake, on the same network, with Developer Mode on?" -ForegroundColor Red
    Write-Host "Kopieren fehlgeschlagen. Ist das Frame an, wach, im selben Netzwerk und der Entwicklermodus an?" -ForegroundColor Red
    Write-Host "IP instead of frame.local / IP statt frame.local:  deploy.ps1 -Frame <IP>"
    Finish 1
}

Write-Host "== Installing / Installiere" -ForegroundColor Cyan
Write-Host "   The first install restarts Steam on the Frame once. / Beim ersten Mal startet Steam auf dem Frame einmal neu."
$remote = 'bash ~/{0}/install-on-frame.sh; rc=$?; rm -rf ~/{0}; exit $rc' -f $tmp
& ssh @opts $target $remote
if ($LASTEXITCODE -ne 0) {
    Write-Host "Installing failed (see above). / Installation fehlgeschlagen (siehe oben)." -ForegroundColor Red
    Finish 1
}
Write-Host ""
Write-Host "Done! On the Frame: Steam > Library > Non-Steam > Just Video" -ForegroundColor Green
Write-Host "Fertig! Auf dem Frame: Steam > Bibliothek > Nicht-Steam > Just Video" -ForegroundColor Green
Finish 0

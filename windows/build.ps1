# Baut Just Video fürs Steam Frame (ARM64) in Docker.
# Start (PowerShell, im Repo-Ordner):
#   powershell -ExecutionPolicy Bypass -File windows\build.ps1
# Ergebnis: out\JustVideo (danach windows\deploy.ps1). Protokoll: out\build.log
# Der erste Build dauert länger (FFmpeg + dav1d werden einmal gebaut und in
# Docker-Volumes aufgehoben), danach nur noch wenige Minuten.
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$image = "just-video-frame-build"

docker version *> $null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Docker läuft nicht. Starte Docker Desktop, warte bis es bereit ist, und starte den Befehl nochmal." -ForegroundColor Red
    exit 1
}

Write-Host "== Build-Umgebung (Docker-Image) vorbereiten" -ForegroundColor Cyan
docker build -t $image -f "$repo\windows\Dockerfile" "$repo\windows"
if ($LASTEXITCODE -ne 0) { Write-Host "Docker-Image konnte nicht gebaut werden." -ForegroundColor Red; exit 1 }

New-Item -ItemType Directory -Force "$repo\out" | Out-Null
Write-Host "== Just Video bauen (Protokoll: out\build.log)" -ForegroundColor Cyan
docker run --rm `
    -v "${repo}:/src:ro" `
    -v "${repo}\out:/out" `
    -v jvf-deps:/build/src/.local-deps `
    -v jvf-target:/build/src/target `
    -v jvf-cargo-registry:/usr/local/cargo/registry `
    -v jvf-cargo-git:/usr/local/cargo/git `
    $image bash /src/windows/container-build.sh
$code = $LASTEXITCODE

if ($code -eq 0) {
    Write-Host ""
    Write-Host "Fertig! Paket: out\JustVideo" -ForegroundColor Green
    Write-Host "Aufs Frame installieren: powershell -ExecutionPolicy Bypass -File windows\deploy.ps1"
} else {
    Write-Host ""
    Write-Host "Build fehlgeschlagen. Details: out\build.log" -ForegroundColor Red
}
exit $code

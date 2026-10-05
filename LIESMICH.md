# Just Video fürs Steam Frame

VR-Videoplayer, der direkt auf dem Steam Frame läuft. Er beruht auf dem
Open-Source-Projekt [Just Video](https://github.com/kumorig/just-video) (MIT)
und bringt dessen offene Community-Erweiterungen mit.

- **Formate:** H.264, H.265/HEVC und VP9 über den Hardware-Decoder des Frames
  (auch 8K), **AV1** über dav1d auf der CPU (4K flüssig).
- **VR:** 180°, 360°, Fisheye und flach, Seite-an-Seite/oben-unten, Augen
  tauschen. Wird am Dateinamen (`_180_LR`, `_3dh`, `_TB` …) oder an den
  Metadaten erkannt, lässt sich pro Video ändern und wird gemerkt.
- **Quellen:** Ordner auf dem Frame („This headset“: Videos, Downloads,
  Home-Ordner, microSD, USB) und Freigaben am PC/NAS (SMB) zum direkten
  Streamen, ohne zu kopieren.
- **Vorschaubilder** in der Liste (Schalter oben in der Ordneransicht),
  Weiterschauen, Untertitel, Tonspuren, Bildkorrektur.

## Bauen (Windows mit Docker Desktop)

Docker Desktop starten, dann in PowerShell im Repo-Ordner:

```powershell
powershell -ExecutionPolicy Bypass -File windows\build.ps1
```

Der erste Build dauert länger (FFmpeg und dav1d werden einmal für das Frame
gebaut), danach nur noch wenige Minuten. Ergebnis: `out\JustVideo`,
Protokoll: `out\build.log`.

## Aufs Frame installieren

1. Auf dem Frame: **Einstellungen > System > Entwicklermodus** einschalten,
   dann unter **Entwickler** ein **Benutzerpasswort** setzen.
2. Am PC:

   ```powershell
   powershell -ExecutionPolicy Bypass -File windows\deploy.ps1
   ```

   Wird `frame.local` nicht gefunden: `windows\deploy.ps1 -Frame <IP des Frames>`.
   Beim ersten Mal startet Steam auf dem Frame einmal kurz neu.
3. Auf dem Frame: **Steam > Bibliothek > Nicht-Steam > Just Video**.

## Videos vom PC streamen

Ordner am PC freigeben (Rechtsklick > Eigenschaften > Freigabe > Erweiterte
Freigabe, nur Lesen reicht). Im Player **Add server** wählen und PC-Adresse,
Windows-Benutzername und Passwort eingeben. Am schnellsten geht es über die
6-GHz-Verbindung des Frame-Adapters am PC.

## Herkunft und Lizenz

MIT, siehe `LICENSE` und `THIRD_PARTY_NOTICES.md`. Zusammengeführt aus
`kumorig/just-video` (main) und den offenen Pull Requests von Nick Vance
(#5–#15) und Leonhard Gruenschloss (#2, #4).

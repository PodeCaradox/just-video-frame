# Just Video fürs Steam Frame

`#SteamFrame` `#VR` `#VR180` `#VR360` `#Videoplayer` `#OpenXR` `#Rust` `#FFmpeg` `#AV1` `#SteamOS`

Ein VR-Videoplayer, der **direkt auf dem Steam Frame** läuft: Zum Abspielen
braucht es keinen PC. Er spielt Videos vom Headset selbst oder direkt aus einer
Freigabe am PC/NAS im Netzwerk, ohne sie vorher zu kopieren.

Das ist ein Community-Fork von [kumorig/just-video](https://github.com/kumorig/just-video)
(MIT-Lizenz). Er führt das Original mit seinen offenen Pull Requests zusammen
und ergänzt einen Windows-Build, einen Installer fürs Headset und eine Liste
aller Videos inklusive Unterordnern. English: [README.md](README.md).

> Inoffiziell und experimentell. Kein Projekt von Valve.

## Was er kann

- **Hardware-Decoding** von H.264, H.265/HEVC und VP9 (8 Bit) über den
  Video-Decoder des Frames, bis 8K (8192×8192).
- **AV1** mit dav1d auf der CPU: 4K mit 60 fps wird schneller als Echtzeit
  dekodiert. 8K-AV1 und 8K-HEVC mit 10 Bit schafft die CPU nicht (der
  Hardware-Decoder kann nur 8 Bit).
- **VR-Formate:** flach, VR180, VR360 und Fisheye; nebeneinander,
  übereinander, Augen getauscht. Erkannt an den Metadaten, an Kürzeln im
  Dateinamen (`_180`, `_360`, `_LR`, `_TB`, `_3dh`, `180x180`, `fisheye190` …)
  und an 2:1-Bildern ab 5,7K. Pro Video änderbar, wird gemerkt.
- **Quellen:**
  - *This headset*: Videos, Downloads, Home-Ordner, SD-Karten und USB-Sticks.
  - *SMB-Freigaben* (Windows-PC, NAS, Samba): direkt übers Netzwerk abgespielt.
- **Browser:** Vorschaubilder (Bild-Button oben), alle Videos eines Ordners und
  seiner Unterordner in einer Liste (Ordner-Button oben), Prüfung vor dem
  Öffnen, ob ein Video abspielbar ist.
- **Wiedergabe:** Weiterschauen, wo du aufgehört hast, nächstes/voriges Video,
  Untertitel (`.srt` neben dem Video und eingebettete), Tonspuren,
  Bildkorrektur.

Dateitypen: `mp4`, `m4v`, `mkv`, `mov`, `webm`, `avi`, `ts`, `m2ts`.

## Bedienung

| Aktion | Taste |
| --- | --- |
| Zeigen / auswählen | Mit dem Controller zielen, Trigger oder **A** |
| Zurück | **B** |
| Liste scrollen | Stick hoch/runter (Griff halten: schneller) |
| Zurück-/Vorspulen | Steuerkreuz links/rechts oder Stick kurz zur Seite (Griff halten: weiter) |
| Lautstärke | Steuerkreuz hoch/runter |
| Bild neu ausrichten | Stick drücken |

Sprunglängen, Lautstärkeschritte und Weiterschauen stellst du unter
**Settings** ein.

## Release installieren (ohne PC)

1. Im Browser des Frames `just-video-frame-…-steamframe-arm64.zip` von der
   Seite **Releases** dieses Repos herunterladen.
2. Auf dem Linux-Desktop: Dolphin → Downloads, Rechtsklick auf die Zip →
   **Entpacken → Archiv entpacken nach …** → Home-Ordner.
3. Den Ordner `JustVideo` öffnen, **F4** drücken (ein Terminal öffnet sich in
   dem Ordner) und `bash install-on-frame.sh` eingeben. Beim ersten Mal startet
   Steam einmal neu.
4. **Bibliothek → Nicht-Steam → Just Video**.

Aktualisieren geht genauso.

## Bauen

### Windows mit Docker Desktop

Docker Desktop starten, dann in PowerShell im Repo-Ordner:

```powershell
powershell -ExecutionPolicy Bypass -File windows\build.ps1
```

Der erste Build baut FFmpeg und dav1d einmal fürs Frame (bleibt in
Docker-Volumes gespeichert), danach dauert es nur noch wenige Minuten.
Ergebnis: `out\JustVideo` und dasselbe als Release-Zip
(`out\just-video-frame-v…-steamframe-arm64.zip`), Protokoll: `out\build.log`.

### Linux

Mit Rust, `curl`, `make`, `ninja`, `pkg-config` und `python3`:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo install cargo-zigbuild
bash scripts/build-frame-media.sh   # einmal: FFmpeg + dav1d fürs Frame
bash scripts/build-frame.sh
```

## Selbst gebaute Version installieren

1. Auf dem Frame: **Einstellungen → System → Entwicklermodus** einschalten,
   dann unter **Entwickler** ein **Benutzerpasswort** setzen.
2. Unter Windows:

   ```powershell
   powershell -ExecutionPolicy Bypass -File windows\deploy.ps1
   # oder, falls frame.local nicht gefunden wird:
   powershell -ExecutionPolicy Bypass -File windows\deploy.ps1 -Frame <IP des Frames>
   ```

   Unter Linux: SSH-Schlüssel für `steamos@frame.local` einrichten und
   `bash scripts/install-frame.sh` ausführen.
3. Auf dem Frame: **Bibliothek → Nicht-Steam → Just Video**.

Beim ersten Mal startet Steam auf dem Headset einmal neu: Der Eintrag muss als
VR-App markiert werden, und Steam liest das nur beim Start. Alternativ kannst
du `out/JustVideo` selbst aufs Headset kopieren und dort in Konsole
`bash install-on-frame.sh` ausführen.

## Videos vom PC streamen

1. Den Videoordner am PC freigeben: Rechtsklick → **Eigenschaften → Freigabe →
   Erweiterte Freigabe → Diesen Ordner freigeben** (Lesen reicht). Dateifreigabe
   muss für das Netzwerk erlaubt sein (Netzwerkprofil „Privat“).
2. In Just Video **Add server** wählen und die Adresse des PCs sowie deinen
   Windows-Benutzernamen und dein Passwort eingeben.

Am besten über die 6-GHz-Verbindung des Frame-Adapters am PC: Sie ist viel
schneller als ein 2,4-GHz-Heimnetz, das bei 8K-Dateien nicht mitkommt.

## Wenn etwas nicht geht

- Protokoll der App: `~/Applications/JustVideo/just-video.log` auf dem Headset.
- Ein 8K-Video öffnet nicht oder läuft auf der CPU: Headset neu starten. Der
  Hardware-Decoder teilt seine Leistung mit anderen offenen Decoder-Sitzungen
  (Steams eigener Web-Helper belegt eine).
- Ein Video ist flach oder doppelt: Format im Player ändern, das wird für die
  Datei gespeichert.

## Herkunft und Lizenz

MIT, siehe [LICENSE](LICENSE). Lizenzen von FFmpeg (LGPL-2.1), dav1d und zlib:
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

- [Just Video](https://github.com/kumorig/just-video) von kumorig: Player,
  SMB-Client, Renderer.
- Hier zusammengeführte offene Pull Requests: Nick Vance (#5–#15:
  Vorschaubilder, Einstellungen, Steuerkreuz, schnelleres Spulen und Lesen über
  SMB, Renderer, Steam-Bibliotheksbilder), Leonhard Gruenschloss (#2: Videos
  auf dem Headset, #4: Half-SBS/OU-Filme).
- Dieser Fork: Windows-/Docker-Build, Installer fürs Headset, Videos in
  Unterordnern.

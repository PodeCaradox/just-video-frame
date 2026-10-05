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

## Installieren

Zwei Wege, beide installieren dieselbe App:

- **A. Direkt auf dem Headset:** Release-Zip im Browser des Frames laden.
  Kein PC und kein Entwicklermodus nötig.
- **B. Vom PC über SSH:** mit eingeschaltetem Entwicklermodus. Praktisch für
  Updates und selbst gebaute Versionen.

### A. Direkt auf dem Headset

1. Im Browser des Frames `just-video-frame-…-steamframe-arm64.zip` von der
   Seite **Releases** dieses Repos herunterladen.
2. Auf dem Linux-Desktop: Dolphin → Downloads, Rechtsklick auf die Zip →
   **Entpacken → Archiv entpacken nach …** → Home-Ordner.
3. Den Ordner `JustVideo` öffnen, **F4** drücken (ein Terminal öffnet sich in
   dem Ordner) und eingeben:

   ```sh
   bash install-on-frame.sh
   ```

   Beim ersten Mal startet Steam einmal neu: Der Eintrag muss als VR-App
   markiert werden, und Steam liest das nur beim Start.
4. **Bibliothek → Nicht-Steam → Just Video**.

### B. Vom PC über SSH

**1. Entwicklermodus einschalten** (auf dem Frame)

1. Steam-Taste → **Einstellungen → System** → **Entwicklermodus aktivieren**
   (*Enable Developer Mode*) einschalten.
2. In den Einstellungen erscheint der Bereich **Entwickler**. Dort
   **Benutzerpasswort festlegen** (*Set User Password*) wählen. Das ist das SSH-Passwort, der
   Benutzername ist `steamos`.

Der Entwicklermodus öffnet SSH und andere Dienste im Netzwerk: nur in
Netzwerken nutzen, denen du vertraust. Just Video braucht ihn nach der
Installation nicht, du kannst ihn danach wieder ausschalten.

**2. Frame finden und Verbindung testen** (am PC)

Die Adresse des Frames ist meist `frame.local`. Wird sie nicht gefunden, nimm
die IP-Adresse (Einstellungen → Internet → das verbundene WLAN). Test in
PowerShell (Windows) oder im Terminal (Linux, macOS):

```sh
ssh steamos@frame.local
```

Die Frage nach dem Fingerabdruck mit `yes` beantworten, Passwort eingeben,
dann `exit`.

**3. Installieren**

| | Release-Zip | Selbst gebaut (dieses Repo) |
| --- | --- | --- |
| Windows | Zip entpacken (Rechtsklick → Alle extrahieren), dann im Ordner `JustVideo` Rechtsklick auf `deploy.ps1` → **Mit PowerShell ausführen** | `powershell -ExecutionPolicy Bypass -File windows\deploy.ps1` |
| Linux, macOS | `bash JustVideo/deploy.sh` | `bash scripts/install-frame.sh` (braucht den SSH-Schlüssel unten) |

Die IP-Adresse anhängen, falls `frame.local` nicht gefunden wird:
`deploy.ps1 -Frame 192.168.1.50`, `deploy.sh 192.168.1.50`,
`FRAME_HOST=192.168.1.50 bash scripts/install-frame.sh`.
Ohne SSH-Schlüssel wird zweimal nach dem Passwort gefragt (Kopieren,
Installieren). Beim ersten Mal startet Steam auf dem Headset einmal neu.

**4. Optional: SSH-Schlüssel statt Passwort**

Windows (PowerShell):

```powershell
ssh-keygen -t ed25519
type $env:USERPROFILE\.ssh\id_ed25519.pub | ssh steamos@frame.local "mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys"
```

Linux, macOS (diesen Schlüssel nutzt `scripts/install-frame.sh`):

```sh
ssh-keygen -t ed25519 -f ~/.ssh/steam_frame_ed25519
ssh-copy-id -i ~/.ssh/steam_frame_ed25519 steamos@frame.local
```

**Wenn SSH nicht verbindet**

- *Could not resolve hostname frame.local*: stattdessen die IP-Adresse nehmen.
- *Connection refused* oder *timed out*: Ist der Entwicklermodus an, das
  Headset wach und im selben Netzwerk wie der PC?
- *Permission denied*: unter **Entwickler** das Passwort festlegen (Schritt 1).
- *Remote host identification has changed* (nach dem Zurücksetzen des
  Headsets): `ssh-keygen -R frame.local`, dann neu verbinden.

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

Danach mit Weg **B** installieren (oder `out/JustVideo` aufs Headset kopieren
und dort Schritt 3 von Weg **A** ausführen).

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

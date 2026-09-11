# Streame

Régie vidéo légère en Rust pour Mac : un téléphone envoie sa caméra et son micro en WebRTC,
le Mac compose des scènes façon OBS sur le GPU (Metal), affiche le programme sur la sortie HDMI,
route l'audio vers une carte son multicanal (Behringer Wing) et renvoie un retour audio au
téléphone. Pilotage par Stream Deck, multiview cliquable, page web de contrôle et API HTTP.

```text
 Téléphone (Safari/Chrome)                     Mac (streame)
 ┌───────────────────────┐   HTTPS + WS      ┌──────────────────────────────────────────────┐
 │ page web /            │◄──────────────────│ serveur axum : page, signaling, /control, API │
 │ caméra + micro        │ ── WebRTC vidéo ─►│ webrtcbin ─► vtdec (NV12) ─► texture GPU       │
 │ retour audio ◄────────│ ◄─ WebRTC audio ─ │                       ▼                       │
 └───────────────────────┘                   │  scènes (wgpu/Metal) ─► programme ─► HDMI      │
                                             │        └─► multiview (fenêtre cliquable)        │
   Stream Deck ─────────────────────────────►│  audio : téléphone ─► Wing (canaux au choix)    │
                                             │          Wing (entrées au choix) ─► téléphone   │
                                             └──────────────────────────────────────────────┘
```

## Fonctionnalités

- **Téléphone → Mac** : le téléphone ouvre `https://<ip-du-mac>:8443/`. Écran de réglages
  (nom, objectif de caméra, source audio, qualité) avec aperçu, puis écran direct épuré à
  trois boutons (micro, son, quitter). Vidéo H264 (ou VP8) + audio Opus en WebRTC, un seul
  téléphone à la fois (une nouvelle connexion remplace la précédente).
- **Objectif et source audio** : la liste des caméras (les objectifs de l'iPhone : grand angle,
  ultra grand angle, téléobjectif) et des entrées audio (micro intégré, AirPods…) est proposée
  après autorisation. La capture est figée en 16:9 paysage ; une invite demande de tourner le
  téléphone en portrait, et l'app s'installe en plein écran via « Sur l'écran d'accueil ».
- **Retour « à l'antenne »** : quand le flux du téléphone est diffusé sur la sortie programme,
  la page affiche un cadre rouge et un badge « À L'ANTENNE ».
- **Retour audio** : les entrées choisies de la carte son sont renvoyées au téléphone (bidirectionnel).
- **Composition GPU** : scènes, fondus et multiview rendus par wgpu (Metal sur macOS), à la
  fréquence de l'écran ; le décodage matériel (VideoToolbox) sort en NV12, converti dans le shader.
- **Sortie HDMI** : fenêtre plein écran sur l'écran de votre choix (nom ou index).
- **Scènes** : calques `phone` (flux du téléphone), `color`, `image` (PNG avec transparence),
  `video` (fichier, en boucle, avec opacité) et `text`. Exemples fournis : noir, direct, habillage
  d'antenne, overlay vidéo.
- **Transitions** : `cut` ou `fade` (durée configurable).
- **Multiview** : fenêtre avec PREVIEW / PROGRAMME et une tuile par scène ; clic = preview,
  double-clic = passage à l'antenne, `Entrée`/`Espace` = TAKE, touches `1`-`9` = scène directe, `F` = plein écran.
- **Stream Deck** : une touche par scène (rouge = à l'antenne, vert = preview) + TAKE ; reconnexion à chaud.
- **Contrôle distant** : `https://<ip>:8443/control` (tablette, second téléphone…) et API HTTP
  (`POST /api/program/<id>`, `/api/cut/<id>`, `/api/preview/<id>`, `/api/take`, `GET /api/state`),
  utilisable depuis Bitfocus Companion par exemple.
- **Audio multicanal** : deux sources mélangées vers la carte son sur des canaux distincts.
  Le son de l'habillage (vidéos d'overlay) et le son du stream WebRTC vont chacun sur les canaux
  choisis de la Wing (matrice `mix-matrix`), et l'entrée choisie est renvoyée au téléphone.
- **VU-mètres** : le multiview affiche les niveaux (dBFS) de chaque source en temps réel
  (élément `level`) : Stream, Habillage et Retour (l'audio renvoyé au téléphone).
- **Sélection audio** : le panneau `/control` liste les périphériques détectés et permet de
  re-router les canaux de chaque source en direct (`GET /api/audio`).

## Installation (macOS)

```bash
# 1. Outils
xcode-select --install
curl https://sh.rustup.rs -sSf | sh          # Rust
brew install gstreamer libnice-gstreamer pkg-config   # GStreamer (formule unifiée) + plugin ICE pour WebRTC

# 2. Compilation
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$PKG_CONFIG_PATH"
cargo build --release

# 3. Vérification des plugins
./target/release/streame check
./target/release/streame devices             # liste cartes son (nom, canaux) et écrans
```

> Homebrew livre le plugin `nice` (ICE, indispensable à `webrtcbin`) dans la formule séparée
> `libnice-gstreamer` ; sans elle, `streame check` signale `nicesrc` manquant. Les avertissements
> `GLib-GIRepository` du scanner de plugins au premier lancement viennent du plugin Python de
> GStreamer et sont sans conséquence. Les binaires GStreamer officiels (gstreamer.freedesktop.org,
> paquet « development ») fonctionnent aussi.

Permissions macOS : au premier lancement, autoriser l'accès **au micro** (carte son) et,
pour le Stream Deck, fermer l'application Elgato (elle monopolise l'appareil).

## Démarrage rapide

```bash
./target/release/streame init            # écrit streame.toml + assets/lower-third.png
$EDITOR streame.toml                     # noms de carte son, écran, canaux, scènes...
./target/release/streame                 # lance la régie
```

Au démarrage, l'URL du téléphone est affichée avec un QR code. Le certificat est auto-signé
(la caméra exige HTTPS) : sur iPhone, accepter l'avertissement Safari (« Afficher les détails »
→ « visiter ce site web »). Le certificat est stocké dans `certs/` et réutilisé.

Options : `--config <fichier>` (défaut `streame.toml`), `--no-window` (sortie par `autovideosink`,
sans fenêtres natives), `RUST_LOG=debug` pour plus de traces, `GST_DEBUG=3` pour GStreamer.

## Configuration (`streame.toml`)

Voir `streame.example.toml` pour un exemple complet. Principales sections :

```toml
[server]
bind = "0.0.0.0:8443"
video_codec = "H264"          # ou "VP8"
h264_profile_level_id = "42e01f"   # "640c1f" = profil High (meilleure qualité sur iPhone récent)
rtc_latency_ms = 120          # jitter buffer WebRTC

[video]
width = 1920                  # taille du canevas des scènes (la sortie suit l'écran)
height = 1080

[output]
display = "HDMI"              # sous-chaîne du nom de l'écran, ou index (voir `streame devices`)
fullscreen = true

[multiview]
enabled = true
width = 1280
height = 720
columns = 4

[audio]
output_device = "WING"        # "default", "none" ou sous-chaîne du nom (voir `streame devices`)
input_device = "WING"
output_channels = 0                # 0 = auto (nombre max de canaux du périphérique)
input_channels = 0
branding_output_channels = [1, 2]  # habillage stéréo → sorties 1/2 de la Wing
stream_output_channels = [3, 4]    # stream WebRTC stéréo → sorties 3/4 (ou [3] pour mono)
return_from_input_channels = [1]   # entrée 1 de la Wing → retour du téléphone
meters = true                      # VU-mètres dans le panneau de contrôle

[transition]
kind = "fade"                 # ou "cut"
duration_ms = 400

[streamdeck]
enabled = true
brightness = 60
buttons = [
  { index = 0, scene = "black" },
  { index = 1, scene = "live" },
  { index = 4, action = "take", label = "TAKE" },   # actions : program (défaut), cut, preview, take
]

[[scenes]]
id = "branding"
name = "Habillage"
layers = [
  { kind = "phone" },
  { kind = "image", path = "assets/lower-third.png" },
  { kind = "text", text = "EN DIRECT", font = "Sans Bold 40", x = 120, y = 900, width = 600, height = 90 },
]

[[scenes]]
id = "overlay"
name = "Overlay vidéo"
layers = [
  { kind = "phone" },
  { kind = "video", path = "assets/overlay.mp4", loop = true, x = 1280, y = 60, width = 576, height = 324, opacity = 0.9 },
]
```

Chaque calque accepte `x`, `y`, `width`, `height` (pixels dans l'image programme ; absents = plein cadre)
et, selon le type, `opacity`, `color` (`#RRGGBB` ou `#RRGGBBAA`), `font`, `loop`.
Les chemins relatifs sont résolus par rapport au fichier de configuration.
Les fichiers vidéo avec couche alpha (ProRes 4444, WebM VP9 alpha…) sont composités avec leur transparence.

## Architecture

| Module | Rôle |
| --- | --- |
| `src/engine.rs` | État de la régie (scènes, programme, preview, transitions), chargement des calques (PNG, texte, fichiers vidéo via `uridecodebin` → `appsink` en boucle), routage audio (`audioconvert mix-matrix`). |
| `src/render.rs` | Rendu wgpu : chaque scène dans une texture hors écran, fondu programme, tuiles/cadres/libellés du multiview, conversion NV12 → RGB dans le shader. |
| `src/frame.rs` | Emplacements d'images partagés entre GStreamer et le rendu (dernière image + compteur i/s). |
| `src/webrtc.rs` | Une session `webrtcbin` par téléphone (le Mac fait l'offre) ; vidéo décodée vers le GPU via `appsink`, audio vers `interaudiosink` ; retour audio encodé en Opus. |
| `src/server.rs` | Serveur HTTPS axum : page téléphone, WebSocket de signaling, page/WebSocket de contrôle, API REST (avec statistiques). |
| `src/ui.rs` | Fenêtres winit : programme (plein écran sur l'écran choisi) et multiview (clics, clavier), rendu cadencé sur la fréquence de l'écran. |
| `src/streamdeck.rs` | Thread Stream Deck (hidapi) : rendu des touches, actions, reconnexion. |
| `src/text.rs` | Rendu de texte (police DejaVu embarquée) pour les libellés et les touches. |
| `src/audio.rs` | Énumération des périphériques (GstDeviceMonitor), matrices de routage. |
| `src/config.rs` | Modèle de configuration TOML. |
| `web/` | Pages téléphone et contrôle (embarquées dans le binaire). |

Les fenêtres sont des surfaces wgpu ; la fenêtre programme suit l'écran HDMI choisi
(`Fullscreen::Borderless`) et cadence le rendu de l'ensemble à la fréquence de cet écran.
Le multiview affiche les statistiques (résolution et i/s du téléphone, i/s du rendu), aussi
disponibles dans `GET /api/state`.

## Limites connues / pistes

- Un seul téléphone simultané (le canal « phone » est unique) ; plusieurs sources = plusieurs canaux à ajouter.
- Les fichiers vidéo sont décodés par GStreamer et envoyés au GPU image par image (suffisant pour des overlays 1080p).
- Pas de son des vidéos d'overlay (volontairement muet), pas d'enregistrement ni de streaming RTMP.
- Le retour audio vers le téléphone est stéréo 48 kHz Opus ; l'annulation d'écho est faite côté téléphone.
- Sur Chrome/Android, forcer `video_codec = "VP8"` si le H264 matériel n'est pas disponible.
- La négociation active l'extension d'en-tête RTP *transport-wide-cc* : sans elle, l'estimation
  de bande passante du téléphone reste bloquée au débit plancher (~300 kb/s) et l'image est très
  dégradée malgré un réseau rapide. Le débit maximal est fixé côté téléphone (`web/app.js`) selon
  la résolution (8 Mb/s en 1080p).

## Tests

Un test de bout en bout (Chromium headless avec caméra simulée) a été utilisé pendant le
développement : offre/réponse SDP, connexion ICE, flux vidéo et audio décodés côté Rust,
retour audio reçu par le navigateur, bascule de scènes via l'API. Le mode `--no-window` lance
le moteur sans fenêtres (serveur, audio et API seulement).

# Streame

Régie vidéo légère en Rust pour Mac : un téléphone envoie sa caméra et son micro en WebRTC,
le Mac compose des scènes façon OBS, affiche le programme sur la sortie HDMI, route l'audio
vers une carte son multicanal (Behringer Wing) et renvoie un retour audio au téléphone.
Pilotage par Stream Deck, multiview cliquable, page web de contrôle et API HTTP.

```text
 Téléphone (Safari/Chrome)                     Mac (streame)
 ┌───────────────────────┐   HTTPS + WS      ┌──────────────────────────────────────────────┐
 │ page web /            │◄──────────────────│ serveur axum : page, signaling, /control, API │
 │ caméra + micro        │ ── WebRTC vidéo ─►│ webrtcbin ─► décodage ─► intervideosink        │
 │ retour audio ◄────────│ ◄─ WebRTC audio ─ │                       ▼                       │
 └───────────────────────┘                   │  scènes (compositor) ─► programme ─► HDMI      │
                                             │        └─► multiview (fenêtre cliquable)        │
   Stream Deck ─────────────────────────────►│  audio : téléphone ─► Wing (canaux au choix)    │
                                             │          Wing (entrées au choix) ─► téléphone   │
                                             └──────────────────────────────────────────────┘
```

## Fonctionnalités

- **Téléphone → Mac** : le téléphone ouvre `https://<ip-du-mac>:8443/`, choisit caméra/qualité et se connecte.
  Vidéo H264 (ou VP8) + audio Opus en WebRTC, signaling par WebSocket, un seul téléphone à la fois
  (une nouvelle connexion remplace la précédente).
- **Retour audio** : les entrées choisies de la carte son sont renvoyées au téléphone (bidirectionnel).
- **Sortie HDMI** : fenêtre plein écran sur l'écran de votre choix, résolution/fréquence configurables.
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
- **Audio multicanal** : matrice de routage (`mix-matrix`) vers n'importe quels canaux de la Wing.

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
rtc_latency_ms = 120          # jitter buffer WebRTC

[video]
width = 1920
height = 1080
fps = 30

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
output_channels = 0           # 0 = auto (nombre max de canaux du périphérique)
input_channels = 0
phone_to_output_channels = [5, 6]     # son du téléphone → canaux 5/6 de la Wing (G, D)
return_from_input_channels = [1, 2]   # entrées 1/2 de la Wing → oreillette du téléphone

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
| `src/engine.rs` | Pipeline GStreamer principal : source téléphone (`intervideosrc`), une `compositor` par scène, mélangeur programme (transitions par alpha/zorder), `input-selector` de preview, `compositor` multiview, routage audio (`audioconvert mix-matrix`), sorties vers les fenêtres (`appsink`). |
| `src/webrtc.rs` | Une session `webrtcbin` par téléphone (le Mac fait l'offre) ; flux décodés poussés dans les canaux `intervideosink`/`interaudiosink` ; retour audio encodé en Opus. |
| `src/server.rs` | Serveur HTTPS axum : page téléphone, WebSocket de signaling, page/WebSocket de contrôle, API REST. |
| `src/ui.rs` | Fenêtres natives winit + softbuffer : programme (plein écran sur l'écran choisi) et multiview (clics, clavier, cadres rouge/vert). |
| `src/streamdeck.rs` | Thread Stream Deck (hidapi) : rendu des touches, actions, reconnexion. |
| `src/audio.rs` | Énumération des périphériques (GstDeviceMonitor), matrices de routage. |
| `src/config.rs` | Modèle de configuration TOML. |
| `web/` | Pages téléphone et contrôle (embarquées dans le binaire). |

Les fenêtres reçoivent des images BGRx déjà à leur taille (mise à l'échelle GStreamer), copiées
dans un tampon `softbuffer` ; la fenêtre programme suit l'écran HDMI choisi (`Fullscreen::Borderless`).

## Limites connues / pistes

- Un seul téléphone simultané (le canal « phone » est unique) ; plusieurs sources = plusieurs canaux à ajouter.
- Les compositors tournent en logiciel (`compositor`) : un Mac Apple Silicon tient 1080p30 avec quelques scènes ;
  pour 4K, passer à `glvideomixer` serait la suite logique.
- Pas de son des vidéos d'overlay (volontairement muet), pas d'enregistrement ni de streaming RTMP.
- Le retour audio vers le téléphone est stéréo 48 kHz Opus ; l'annulation d'écho est faite côté téléphone.
- Sur Chrome/Android, forcer `video_codec = "VP8"` si le H264 matériel n'est pas disponible.

## Tests

Un test de bout en bout (Chromium headless avec caméra simulée) a été utilisé pendant le
développement : offre/réponse SDP, connexion ICE, flux vidéo et audio décodés côté Rust,
retour audio reçu par le navigateur, bascule de scènes via l'API. Sous Linux, le mode
`--no-window` (ou l'option cachée `--fake-output`) permet de lancer le moteur sans écran.

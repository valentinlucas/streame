# Streame

Régie vidéo légère en Rust pour Mac : un téléphone envoie sa caméra et son micro en WebRTC,
le Mac compose des scènes façon OBS sur le GPU (Metal), affiche le programme sur la sortie HDMI,
route l'audio vers une carte son multicanal (Behringer Wing) et renvoie un retour audio au
téléphone. Pilotage par Stream Deck, multiview cliquable, page web de contrôle et API HTTP.

```text
 Téléphone (Safari/Chrome)                     Mac (streame)
 ┌───────────────────────┐   HTTPS + WS      ┌──────────────────────────────────────────────┐
 │ page web /            │◄──────────────────│ serveur axum : page, signaling, /control, API │
 │ caméra + micro        │ ── WebRTC vidéo ─►│ webrtc-rs ─► vtdec (NV12) ─► texture GPU        │
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
- **VU-mètres** : les niveaux (dBFS) de chaque source (Stream, Habillage, Retour vers le
  téléphone) s'affichent en temps réel (élément `level`) à la fois dans le multiview et dans
  le panneau `/control`.
- **Sélection audio** : le panneau `/control` liste les périphériques détectés et permet de
  re-router les canaux de chaque source en direct (`GET /api/audio`).

## Installation (macOS)

```bash
# 1. Outils
xcode-select --install
curl https://sh.rustup.rs -sSf | sh          # Rust
brew install gstreamer pkg-config   # GStreamer (formule unifiée) pour le décodage/encodage

# 2. Compilation
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$PKG_CONFIG_PATH"
cargo build --release

# 3. Vérification des plugins
./target/release/streame check
./target/release/streame devices             # liste cartes son (nom, canaux) et écrans
```

> Le transport WebRTC (ICE/DTLS/SRTP) est assuré par **webrtc-rs** (crate `webrtc` + cœur
> sans-I/O `rtc`), plus par GStreamer : `libnice-gstreamer`/`webrtcbin` ne sont donc plus
> nécessaires. GStreamer ne sert qu'au décodage matériel et aux codecs. Les avertissements
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
rtc_latency_ms = 60           # jitter buffer WebRTC (baisser sur LAN propre = moins de latence)

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
| `src/webrtc.rs` | Une session **webrtc-rs** par téléphone (le Mac fait l'offre, H264 en tête des codecs) : ICE/DTLS/SRTP/RTP, jitter buffer et TWCC/NACK en Rust. Vidéo : RTP → `appsrc` → `decodebin` (vtdec) → GPU, un pipeline isolé par piste. Audio : décodage Opus par **libopus** (PLC/FEC) → mixeur cpal ; retour encodé par libopus au rythme de la carte, écrit sur la piste locale. Image-clé demandée par rafale au démarrage puis seulement si les images cessent. |
| `src/server.rs` | Serveur HTTPS axum : page téléphone, WebSocket de signaling, page/WebSocket de contrôle, API REST (avec statistiques). |
| `src/ui.rs` | Fenêtres winit : programme (plein écran sur l'écran choisi) et multiview (clics, clavier), rendu cadencé sur la fréquence de l'écran. |
| `src/streamdeck.rs` | Thread Stream Deck (hidapi) : rendu des touches, actions, reconnexion. |
| `src/text.rs` | Rendu de texte (police DejaVu embarquée) pour les libellés et les touches. |
| `src/audio.rs` | Énumération des périphériques, matrices de routage ; sur macOS l'E/S CoreAudio (cpal) : mixage/routage/VU-mètres dans le callback temps réel, pré-tampon et **compensation de dérive d'horloge** (ré-échantillonnage asservi au remplissage de l'anneau). |
| `src/config.rs` | Modèle de configuration TOML. |
| `web/` | Pages téléphone et contrôle (embarquées dans le binaire). |

Les fenêtres sont des surfaces wgpu ; la fenêtre programme suit l'écran HDMI choisi
(`Fullscreen::Borderless`) et cadence le rendu de l'ensemble à la fréquence de cet écran.
Le multiview affiche les statistiques (résolution et i/s du téléphone, i/s du rendu), aussi
disponibles dans `GET /api/state`.

## Limites connues / pistes

- Un seul téléphone simultané (le canal « phone » est unique) ; plusieurs sources = plusieurs canaux à ajouter.
- Les fichiers vidéo sont décodés par GStreamer et envoyés au GPU image par image (suffisant pour des overlays 1080p).
- Pas d'enregistrement ni de streaming RTMP.
- **Audio via CoreAudio (macOS)** : sur macOS, **toute la chaîne carte passe par CoreAudio (cpal)**.
  Le mixage des deux sources (stream du téléphone et habillage), leur routage sur les canaux
  choisis et les VU-mètres sont calculés directement dans le callback temps réel de la carte, à
  son horloge exacte — pas de mélangeur GStreamer, une seule horloge, synchro et latence
  minimales. **L'audio ne passe plus du tout par GStreamer** : le son du téléphone est décodé par
  libopus (avec dissimulation de pertes PLC/FEC) et poussé dans le mixeur, le retour est capturé
  par cpal et encodé par libopus au rythme de la carte (une seule horloge sur ce chemin, FEC en
  bande activé). GStreamer ne sert plus qu'au décodage vidéo et aux fichiers d'habillage ; il ne
  touche plus le périphérique, ce qui supprime le conflit à deux frameworks qui coinçait la Wing.
  Côté sortie, la source (réseau) et la carte ont deux horloges 48 kHz indépendantes : un
  ré-échantillonneur asservi au remplissage de l'anneau (cible ~70 ms, ±0,5 % max) compense la
  dérive en douceur, sans saut de trames ni coupure. La sortie ouvre autant de canaux que la
  carte en expose (8 sur la Wing), l'entrée alimente le retour vers le téléphone. Les flux sont
  fermés proprement à l'arrêt (Ctrl-C, fermeture) pour ne pas laisser la carte USB bloquée. Hors
  macOS, c'est GStreamer qui fait le mixage, les codecs audio et l'E/S.
- Le retour audio vers le téléphone est stéréo 48 kHz Opus ; l'annulation d'écho est faite côté téléphone.
- Sur Chrome/Android, forcer `video_codec = "VP8"` si le H264 matériel n'est pas disponible.
- **Transport WebRTC via webrtc-rs** (v0.21) : `register_default_interceptors` active NACK, les
  rapports RTCP et le *transport-wide-cc* (TWCC) — l'extension d'en-tête RTP est déclarée
  automatiquement. Sans TWCC, l'estimation de bande passante du téléphone reste bloquée au débit
  plancher (~300 kb/s) et l'image est très dégradée malgré un réseau rapide. Le jitter buffer
  adaptatif de webrtc-rs lisse le flux entrant (profondeur = `rtc_latency_ms`) avant le décodage.
  Le débit maximal est fixé côté téléphone (`web/app.js`) selon la résolution (8 Mb/s en 1080p).
  Le mode mDNS *QueryOnly* résout les candidats `.local` d'iOS/Safari pour l'ICE sur le LAN.
- **Ordre des codecs** : webrtc-rs offre VP8 en premier par défaut, et les navigateurs suivent
  l'ordre de l'offre — un iPhone encodait alors en VP8 (logiciel), sans décodage matériel côté
  Mac. L'offre met désormais le codec de `video_codec` en tête (H264 par défaut : encodage
  matériel sur le téléphone, VideoToolbox ici), VP8/VP9 en repli.
- **Sortie audio du téléphone** : la page propose la sortie du retour (`setSinkId`) là où le
  navigateur le permet (Chrome/Brave/Edge/Firefox, Android, desktop). Sur iOS/WebKit la sortie
  suit la route système : choisir les AirPods comme micro les fait devenir la route ; sinon,
  Centre de contrôle. Le micro peut être changé pendant le direct (`replaceTrack`, sans
  renégociation).
- **Décodage vidéo explicite** : `appsrc → rtph264depay → h264parse → vtdec → queue →
  videoconvert → GPU`, un pipeline par piste, câblé avant le démarrage — déterministe et sans
  la `multiqueue` de decodebin. Le dépayloadeur est en `request-keyframe`/`wait-for-keyframe` :
  sur une perte non rattrapée par NACK il demande une image-clé (relayée en PLI) et jette les
  images jusqu'à elle — bref gel plutôt qu'image pixellisée qui se propage.
- **Images-clés** : rafale au démarrage, puis à la demande (discontinuité, plus d'images) et un
  filet de sécurité lent (`keyframe_interval_s`, 10 s par défaut, 0 = off). Pas d'image-clé
  rapprochée : chacune est lourde et fait osciller la qualité.
- **Lip-sync** : `video.av_offset_ms` (80 ms par défaut) retarde l'affichage de la vidéo pour
  l'aligner sur l'audio, dont la lecture est tamponnée (~70 ms + une trame). 0 = au plus tôt.
  L'alignement est « par construction » (budgets de tampon égalisés), pas par RTCP : à régler à
  l'œil si besoin.
- **Jitter buffer** : une seule profondeur (`rtc_latency_ms`, 60 ms) pour audio et vidéo —
  webrtc-rs 0.21 n'en propose pas une par média ; elle doit couvrir l'aller-retour NACK. En
  pratique la vidéo s'affiche dès la sortie du buffer et l'audio a en plus son propre tampon
  adaptatif de lecture (compensation de dérive), donc les latences effectives diffèrent déjà.
- **Mesure de latence verre à verre** : `multiview.clock = true` affiche l'horloge du Mac (UTC,
  ms) dans le bandeau du multiview ; la page `https://<mac>:8443/latency` affiche la même
  horloge calée sur le Mac. Filmer cette page avec le téléphone : l'écart entre l'heure dans
  l'image décodée et celle du bandeau = latence verre à verre. CoreAudio est ouvert avec un
  tampon de 256 trames (~5 ms) quand la carte l'annonce.
- **Robustesse / non-blocage** : deux runtimes Tokio — `server` (HTTPS, WebSocket, signaling) et
  `media` (webrtc-rs, boucles RTP, Opus, décodage) — pour qu'une charge ou un blocage média ne
  rende jamais la page inaccessible. Les arrêts GStreamer (`set_state(Null)`, bloquants) se font
  sur un thread dédié, jamais sur un worker. Un chien de garde journalise tout retard de réveil
  d'un runtime (`runtime … : réveil en retard`) : c'est le signal à chercher si le serveur
  « ne répond plus ». Les WebSocket sont pingés toutes les 15 s. Sur un échec ICE, le Mac
  **relance l'ICE** (nouvelle offre `ice_restart`, pistes et décodeurs conservés, la page répond
  sur la même RTCPeerConnection) — deux tentatives avec 15 s de grâce — au lieu de détruire la
  session ; les états ICE/WebRTC et les erreurs de candidats sont journalisés pour diagnostiquer
  les déconnexions.

## Tests

Un test de bout en bout (Chromium headless avec caméra simulée) a été utilisé pendant le
développement : offre/réponse SDP, connexion ICE, flux vidéo et audio décodés côté Rust,
retour audio reçu par le navigateur, bascule de scènes via l'API. Le mode `--no-window` lance
le moteur sans fenêtres (serveur, audio et API seulement).

# Streame

Régie vidéo légère en Rust pour Mac : un téléphone envoie sa caméra et son micro en WebRTC,
le Mac compose des scènes façon OBS sur le GPU (Metal), affiche le programme sur la sortie HDMI,
route l'audio vers une carte son multicanal (Behringer Wing) et renvoie un retour audio au
téléphone. Pilotage par Stream Deck, multiview cliquable, page web de contrôle et API HTTP.

```text
 Téléphone (Safari/Chrome)                     Mac (streame)
 ┌───────────────────────┐   HTTPS + WS      ┌──────────────────────────────────────────────┐
 │ page web /            │◄──────────────────│ serveur axum : page, signaling, /control, API │
 │ caméra + micro        │ ── WebRTC vidéo ─►│ webrtc-rs ─► VideoToolbox ─► IOSurface (GPU)    │
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
  trois boutons (micro, son, quitter). Vidéo H264 + audio Opus en WebRTC, un seul
  téléphone à la fois (une nouvelle connexion remplace la précédente).
- **App iOS native** (`ios/`) : même fonction que la page, mais avec **le même code Rust que
  la régie** (crate `crates/streame-rtc` : webrtc-rs, libopus, VideoToolbox, cpal) — pas de
  libwebrtc. Découverte de la régie en Bonjour (`_streame._tcp`), contrôle de congestion GCC,
  images-clés à la demande. Voir `ios/README.md`.
- **Objectif et source audio** : la liste des caméras (les objectifs de l'iPhone : grand angle,
  ultra grand angle, téléobjectif) et des entrées audio (micro intégré, AirPods…) est proposée
  après autorisation. La capture est figée en 16:9 paysage ; une invite demande de tourner le
  téléphone en portrait, et l'app s'installe en plein écran via « Sur l'écran d'accueil ».
- **Retour « à l'antenne »** : quand le flux du téléphone est diffusé sur la sortie programme,
  la page affiche un cadre rouge et un badge « À L'ANTENNE ».
- **Retour audio** : les entrées choisies de la carte son sont renvoyées au téléphone (bidirectionnel).
- **Chaîne média 100 % native, zéro copie** : aucun GStreamer. Le H264 du téléphone est décodé
  en matériel par **VideoToolbox**, les fichiers d'habillage par **AVFoundation** ; les deux
  livrent des images NV12 en **IOSurface** que wgpu/Metal importe directement comme textures —
  le CPU ne touche jamais aux pixels. Composition, fondus et multiview sur le GPU à la
  fréquence de l'écran (NV12 → RGB dans le shader).
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
- **Pilotage OSC (QLab)** : les mêmes actions en OSC sur UDP (`/streame/program <id>`,
  `/streame/cut <id>`, `/streame/preview <id>`, `/streame/take`), pour qu'une conduite QLab
  (cues Réseau, licence Audio suffisante) lance le son du générique et bascule les scènes vidéo
  au même instant. Voir « Pilotage depuis QLab ».
- **Génériques et boucles** : un calque vidéo peut lire un fichier une fois puis en boucler un
  second (`then`), ou boucler une plage d'un seul fichier (`loop_from_ms`/`loop_to_ms`) ;
  la lecture (re)démarre quand la scène passe à l'antenne et s'arrête quand elle la quitte.
  Les fichiers avec **alpha** (ProRes 4444, HEVC alpha) sont détectés et superposés au
  téléphone avec transparence ; `audio = false` ignore leur son (QLab le joue).
- **Audio multicanal** : deux sources mélangées vers la carte son sur des canaux distincts.
  Le son de l'habillage (vidéos d'overlay) et le son du stream WebRTC vont chacun sur les canaux
  choisis de la Wing, et l'entrée choisie est renvoyée au téléphone.
- **VU-mètres** : les niveaux (dBFS) de chaque source (Stream, Habillage, Retour vers le
  téléphone) s'affichent en temps réel (calculés dans le callback CoreAudio) à la fois dans le
  multiview et dans le panneau `/control`.
- **Sélection audio** : le panneau `/control` liste les périphériques détectés et permet de
  re-router les canaux de chaque source en direct (`GET /api/audio`).

## Installation (macOS)

```bash
# 1. Outils
xcode-select --install
curl https://sh.rustup.rs -sSf | sh          # Rust
brew install opus pkg-config                 # libopus (codec audio) — seule dépendance externe

# 2. Compilation
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$PKG_CONFIG_PATH"
cargo build --release

# 3. Vérification
./target/release/streame devices             # liste cartes son (nom, canaux) et écrans
```

> Plus de GStreamer : le transport WebRTC (ICE/DTLS/SRTP) est fait par **webrtc-rs** (crate
> `webrtc` + cœur sans-I/O `rtc`), le décodage vidéo par **VideoToolbox** (téléphone) et
> **AVFoundation** (fichiers), l'audio par **libopus** et **CoreAudio** (cpal). L'application est
> donc macOS uniquement.

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

Options : `--config <fichier>` (défaut `streame.toml`), `--no-window` (serveur, audio et API
seulement, sans fenêtres), `RUST_LOG=info,streame=debug` pour plus de traces.

## Configuration (`streame.toml`)

Voir `streame.example.toml` pour un exemple complet. Principales sections :

```toml
[server]
bind = "0.0.0.0:8443"
stun_server = ""              # vide sur un réseau local ; un STUN ne sert que hors LAN (et rarement sans TURN)
video_codec = "H264"          # ou "VP8"
h264_profile_level_id = "42e01f"   # "640c1f" = profil High (meilleure qualité sur iPhone récent)
video_start_bitrate_kbps = 3000    # débit de départ annoncé au téléphone (0 = 300 kb/s de libwebrtc)
video_max_bitrate_kbps = 8000      # plafond en 1080p (720p = 56 %, 480p = 25 %) ; libwebrtc seul : 2,5 Mb/s
mdns = true                        # annonce Bonjour _streame._tcp pour l'app iOS
name = ""                          # nom de la régie dans l'app (vide = nom du Mac)

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
branding_output_channels = [1, 2]  # habillage stéréo → sorties 1/2 de la Wing ([] = non routé, ex. son joué par QLab)
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
Les fichiers vidéo sont lus par AVFoundation (tout format QuickTime/MP4 lisible par le système, H264/HEVC/ProRes…).

Options du calque `video` :

| Option | Effet |
|---|---|
| `loop` | le dernier fichier est lu en boucle (défaut `true`) |
| `then` | fichier lu en boucle après `path`, lu une fois (générique puis boucle) |
| `loop_from_ms`, `loop_to_ms` | un seul fichier : lu de 0 à `loop_to_ms`, puis boucle entre les deux bornes (`loop_to_ms` absent = fin) |
| `start` | `on_air` : (re)démarre quand la scène passe à l'antenne, s'arrête en la quittant (défaut dès que `then` ou une plage est donné) ; `always` : tourne dès le lancement |
| `audio` | `false` : son du fichier ignoré (par exemple joué par QLab) |
| `alpha` | force (`true`) ou interdit (`false`) le décodage avec transparence ; absent = détecté d'après le fichier (ProRes 4444, HEVC alpha : BGRA, alpha direct) |

```toml
[[scenes]]
id = "generique"
name = "Générique"
layers = [
  { kind = "video", path = "assets/generique.mov", then = "assets/generique-boucle.mov", audio = false },
]

[[scenes]]
id = "habillage-alpha"
name = "Habillage alpha"
layers = [
  { kind = "phone" },
  { kind = "video", path = "assets/habillage-intro.mov", then = "assets/habillage-boucle.mov", audio = false },
]
```

## Pilotage depuis QLab

QLab (licence Audio) joue le son et pilote streame en OSC : streame écoute en UDP sur
`osc.bind` (défaut `0.0.0.0:53100`).

1. Dans QLab, *Réglages de l'espace de travail → Réseau* : ajouter un patch **OSC**, protocole
   UDP, adresse IP du Mac de régie (ou `127.0.0.1` si QLab tourne dessus), port `53100`.
2. Créer un cue **Réseau** sur ce patch, type *message OSC*, avec par exemple :

   | Message | Effet |
   |---|---|
   | `/streame/program generique` | passe la scène `generique` à l'antenne (transition configurée) |
   | `/streame/cut black` | noir immédiat |
   | `/streame/program 2` | deuxième scène de la configuration |
   | `/streame/preview live` puis `/streame/take` | preview puis TAKE |

3. Grouper ce cue avec le cue Audio du générique (démarrage simultané) : la scène démarre sa
   vidéo au même instant, lit le générique une fois puis boucle jusqu'à la prochaine scène.
   Redemander une scène déjà à l'antenne relance son générique.

Les mêmes messages fonctionnent depuis Companion ou TouchOSC ; l'API HTTP reste disponible.

## Architecture

| Module | Rôle |
| --- | --- |
| `src/engine.rs` | État de la régie (scènes, programme, preview, transitions), chargement des calques (PNG, texte, fichiers vidéo via `avf.rs`), mise en place de l'audio CoreAudio et du bus d'habillage, routage à chaud. |
| `src/vt.rs` | Décodage H264 **matériel** (VideoToolbox) des unités d'accès du téléphone → `CVPixelBuffer` NV12 sur IOSurface ; recréation de session sur changement de SPS/PPS, attente d'IDR après perte ou erreur. |
| `src/avf.rs` | Lecture des fichiers d'habillage par **AVFoundation** (`AVAssetReader`, décodage matériel) : images NV12 sur IOSurface cadencées sur l'horloge murale, son PCM F32 48 kHz vers le bus d'habillage, boucle à temps continu. |
| `src/render.rs` | Rendu wgpu : import zéro copie des IOSurfaces (texture Metal par plan → `create_texture_from_hal`), chaque scène dans une texture hors écran, fondu programme, tuiles/cadres/libellés du multiview, conversion NV12 → RGB dans le shader. |
| `src/frame.rs` | Emplacements d'images partagés entre les décodeurs et le rendu : `SurfaceFrame` (CVPixelBuffer + IOSurface, gardé en vie tant qu'une texture l'utilise), dernière image + compteur i/s. |
| `crates/streame-rtc/` | **Code média partagé avec l'app iOS** : messages de signaling, construction de la `PeerConnection` webrtc-rs, piste/encodeur/décodeur Opus (libopus, cadencé par la source, PLC/FEC), paramètres H264 ; feature `client` = session téléphone complète (répondeur WebRTC + GCC, encodeur VideoToolbox, audio cpal, WebSocket TLS) et banc `examples/fake_phone.rs`. |
| `crates/streame-ios/` | Bibliothèque statique (API C) de la session téléphone pour l'app iOS (`ios/`). |
| `src/discovery.rs` | Annonce Bonjour `_streame._tcp` (mdns-sd) pour que l'app iOS trouve la régie. |
| `src/webrtc.rs` | Une session **webrtc-rs** par téléphone (le Mac fait l'offre, H264 seul) : ICE/DTLS/SRTP/RTP et TWCC/NACK en Rust. Vidéo : RTP → thread `h264-decode` (remise en ordre + dépaquetisation H264 par `SampleBuilder`) → `vt.rs` → IOSurface → GPU. Audio : décodage Opus par **libopus** (PLC/FEC) → mixeur cpal ; retour encodé par libopus au rythme de la carte, écrit sur la piste locale. Image-clé demandée par rafale au démarrage puis à la demande (perte, erreur, famine, filet périodique). |
| `src/server.rs` | Serveur HTTPS axum : page téléphone, WebSocket de signaling, page/WebSocket de contrôle, API REST (avec statistiques). |
| `src/ui.rs` | Fenêtres winit : programme (plein écran sur l'écran choisi) et multiview (clics, clavier), rendu cadencé sur la fréquence de l'écran. |
| `src/streamdeck.rs` | Thread Stream Deck (hidapi) : rendu des touches, actions, reconnexion. |
| `src/text.rs` | Rendu de texte (police DejaVu embarquée) pour les libellés et les touches. |
| `src/audio.rs` | Énumération des périphériques (cpal), bus d'habillage (somme des sons des vidéos, pas de 10 ms), E/S CoreAudio : mixage/routage/VU-mètres dans le callback temps réel, pré-tampon et **compensation de dérive d'horloge** (ré-échantillonnage asservi au remplissage de l'anneau). |
| `src/config.rs` | Modèle de configuration TOML. |
| `web/` | Pages téléphone et contrôle (embarquées dans le binaire). |
| `ios/` | App iOS native (SwiftUI + bibliothèque Rust) : voir `ios/README.md`. |

Les fenêtres sont des surfaces wgpu ; la fenêtre programme suit l'écran HDMI choisi
(`Fullscreen::Borderless`) et cadence le rendu de l'ensemble à la fréquence de cet écran.
Le multiview affiche les statistiques (résolution et i/s du téléphone, i/s du rendu), aussi
disponibles dans `GET /api/state`.

## Limites connues / pistes

- Un seul téléphone simultané (le canal « phone » est unique) ; plusieurs sources = plusieurs canaux à ajouter.
- **Fichiers d'habillage (AVFoundation)** : `AVAssetReader` décode en matériel et livre des
  IOSurfaces NV12, importées telles quelles par le GPU. Un thread par fichier présente les images
  sur l'horloge murale (retard fixe de 80 ms pour s'aligner sur le son, qui traverse le bus et
  l'anneau de sortie), lit le son ~300 ms en avance et le pousse dans le bus d'habillage ; la
  boucle recrée le lecteur en conservant un temps continu (aucune coupure, 0 image sautée à 25
  i/s sur un M4 Pro). Un calque enchaîne des segments (générique puis boucle : deux fichiers ou
  une plage d'un seul, via `AVAssetReader.timeRange`) et peut être piloté par l'antenne : le
  lecteur reste armé sur la première image (décodée d'avance, visible en preview) pour un départ
  instantané au passage à l'antenne, et le passage suivant est pré-ouvert pendant la lecture
  (couture de boucle sans attente). Les
  fichiers avec alpha sont décodés en BGRA (alpha direct, tel que livré par AVFoundation) et
  mélangés par le shader ; les autres restent en NV12.
- Pas d'enregistrement ni de streaming RTMP.
- **Audio via CoreAudio (macOS)** : sur macOS, **toute la chaîne carte passe par CoreAudio (cpal)**.
  Le mixage des deux sources (stream du téléphone et habillage), leur routage sur les canaux
  choisis et les VU-mètres sont calculés directement dans le callback temps réel de la carte, à
  son horloge exacte — pas de mélangeur GStreamer, une seule horloge, synchro et latence
  minimales. Le son du téléphone est décodé par libopus (avec dissimulation de pertes PLC/FEC)
  et poussé dans le mixeur, le retour est capturé par cpal et encodé par libopus au rythme de la
  carte (une seule horloge sur ce chemin, FEC en bande activé). Le son des vidéos d'habillage est
  sommé par un bus cadencé à 10 ms (flux continu, silence compris, pour que l'anneau reste
  amorcé). Un seul framework ouvre la carte : fini le conflit qui coinçait la Wing.
  Côté sortie, la source (réseau) et la carte ont deux horloges 48 kHz indépendantes : un
  ré-échantillonneur asservi au remplissage de l'anneau (cible ~70 ms, ±0,5 % max) compense la
  dérive en douceur, sans saut de trames ni coupure. La sortie ouvre autant de canaux que la
  carte en expose (8 sur la Wing), l'entrée alimente le retour vers le téléphone. Les flux sont
  fermés proprement à l'arrêt (Ctrl-C, fermeture) pour ne pas laisser la carte USB bloquée.
- Le retour audio vers le téléphone est stéréo 48 kHz Opus ; l'annulation d'écho est faite côté téléphone.
- **H264 uniquement** : VideoToolbox ne décode ni VP8 ni VP9, donc l'offre ne propose que H264
  (`packetization-mode=1`, profil `h264_profile_level_id` en tête). Tout iPhone et tout Chrome
  (encodeur logiciel OpenH264 au pire) le fournit.
- **Transport WebRTC via webrtc-rs** (v0.21) : `register_default_interceptors` active NACK, les
  rapports RTCP et le *transport-wide-cc* (TWCC) — l'extension d'en-tête RTP est déclarée
  automatiquement. Sans TWCC, l'estimation de bande passante du téléphone reste bloquée au débit
  plancher (~300 kb/s) et l'image est très dégradée malgré un réseau rapide.
  Le débit maximal (`video_max_bitrate_kbps`, 8 Mb/s en 1080p) est lu par la page sur
  `/api/config` et appliqué via `setParameters` ; sans plafond explicite, libwebrtc se limite
  à 2,5 Mb/s au-delà de 960×540.
  Le mode mDNS *QueryOnly* résout les candidats `.local` d'iOS/Safari pour l'ICE sur le LAN.
- **Ordre des codecs** : webrtc-rs offre VP8 en premier par défaut, et les navigateurs suivent
  l'ordre de l'offre — un iPhone encodait alors en VP8 (logiciel), sans décodage matériel côté
  Mac. D'où l'offre restreinte à H264 (encodage matériel sur le téléphone, VideoToolbox ici).
- **Sortie audio du téléphone** : la page propose la sortie du retour (`setSinkId`) là où le
  navigateur le permet (Chrome/Brave/Edge/Firefox, Android, desktop). Sur iOS/WebKit la sortie
  suit la route système : choisir les AirPods comme micro les fait devenir la route ; sinon,
  Centre de contrôle. Le micro peut être changé pendant le direct (`replaceTrack`, sans
  renégociation).
- **Décodage vidéo natif zéro copie** : sur un thread dédié `h264-decode`, le `SampleBuilder`
  de webrtc-rs remet les paquets en ordre (fenêtre de 200 ms, qui couvre une retransmission NACK ;
  `max_late` = 2048 paquets, car il se compte en paquets non consommés et doit dépasser la plus
  grosse image-clé) et réassemble les unités d'accès H264 (Annex-B) ; `vt.rs` les soumet
  à une `VTDecompressionSession` (session recréée si SPS/PPS changent) qui rend des
  `CVPixelBuffer` NV12 sur IOSurface ; `render.rs` en fait des textures Metal (`texture_from_raw`
  + `create_texture_from_hal`) sans copie, en gardant le tampon vivant tant que la texture
  l'utilise. Sur une perte non rattrapée par NACK ou une erreur du décodeur, on demande une
  image-clé (PLI) et on jette tout jusqu'à l'IDR — bref gel plutôt qu'image pixellisée.
- **Images-clés** : une demande au démarrage, répétée toutes les 500 ms tant que rien n'est
  décodé, puis à la demande (perte non rattrapée, erreur du décodeur, plus d'images) et un filet
  de sécurité lent (`keyframe_interval_s`, 10 s par défaut, 0 = off). Pas de rafale ni d'image-clé
  rapprochée : chacune coûte une image 1080p entière au téléphone, fait osciller la qualité et,
  au démarrage, engorge son pacer alors que son estimation de débit part de 300 kb/s.
- **Montée en débit** : libwebrtc part de 300 kb/s et monte de ~8 % par seconde, soit ~30 s pour
  atteindre 8 Mb/s avec une cadence dégradée entre-temps. `video_start_bitrate_kbps` (3000 par
  défaut) est annoncé dans l'offre (`x-google-start-bitrate`, lu par Chrome et Safari) pour
  démarrer bien plus haut. Le plafond est `video_max_bitrate_kbps` (8 Mb/s en 1080p) : au-delà
  le gain visuel est faible et chaque image-clé devient une rafale que le Wi-Fi encaisse mal.
- **Lip-sync** : `video.av_offset_ms` (80 ms par défaut) retarde l'affichage de la vidéo pour
  l'aligner sur l'audio, dont la lecture est tamponnée (~70 ms + une trame). 0 = au plus tôt.
  L'alignement est « par construction » (budgets de tampon égalisés), pas par RTCP : à régler à
  l'œil si besoin.
- **Pas de jitter buffer paquet** : comme libwebrtc, la remise en ordre vidéo est faite par
  l'assembleur d'images et la présentation par `av_offset_ms` ; l'audio a son tampon adaptatif de
  lecture (un paquet Opus arrivé dans le désordre est ignoré, sa trame ayant déjà été dissimulée
  par le PLC). Le jitter buffer de webrtc-rs 0.21 donnait à tous les paquets d'une même image la
  même échéance et libérait une image-clé de plusieurs centaines de paquets d'un bloc, ce qui
  débordait le canal borné (256 paquets) entre son pilote et la piste : paquets jetés, image
  cassée, PLI, nouvelle image-clé cassée… et le téléphone finissait bridé à quelques images par
  seconde et ~0,5 Mb/s. Symptôme à surveiller sur `/control` : compteur PLI qui grimpe (attendu :
  6 au démarrage puis 1 toutes les `keyframe_interval_s`).
- **Mesure de latence verre à verre** : `multiview.clock = true` affiche l'horloge du Mac (UTC,
  ms) dans le bandeau du multiview ; la page `https://<mac>:8443/latency` affiche la même
  horloge calée sur le Mac. Filmer cette page avec le téléphone : l'écart entre l'heure dans
  l'image décodée et celle du bandeau = latence verre à verre. CoreAudio est ouvert avec un
  tampon de 256 trames (~5 ms) quand la carte l'annonce.
- **Robustesse / non-blocage** : deux runtimes Tokio — `server` (HTTPS, WebSocket, signaling) et
  `media` (webrtc-rs, boucles RTP, Opus, décodage) — pour qu'une charge ou un blocage média ne
  rende jamais la page inaccessible ; la fermeture d'une session est non bloquante (jeton
  d'annulation, `close()` sur le runtime média). Un chien de garde journalise tout retard de réveil
  d'un runtime (`runtime … : réveil en retard`) : c'est le signal à chercher si le serveur
  « ne répond plus ». Les WebSocket sont pingés toutes les 15 s. Sur un échec ICE, le Mac
  **relance l'ICE** (nouvelle offre `ice_restart`, pistes et décodeurs conservés, la page répond
  sur la même RTCPeerConnection) — deux tentatives avec 15 s de grâce — au lieu de détruire la
  session ; les états ICE/WebRTC et les erreurs de candidats sont journalisés pour diagnostiquer
  les déconnexions.

## Tests

Un test de bout en bout (Chromium headless avec caméra simulée, `scripts/fake-phone.mjs`) a été
utilisé pendant le développement : offre/réponse SDP, connexion ICE, flux vidéo et audio décodés
côté Rust, retour audio reçu par le navigateur, bascule de scènes via l'API. Le mode
`--no-window` lance le moteur sans fenêtres (serveur, audio et API seulement).

Le même parcours existe en Rust pur, avec le code de l'app iOS (`crates/streame-rtc`, feature
`client`) : mire animée encodée par VideoToolbox, micro synthétique, GCC, statistiques.

```bash
./target/debug/streame --no-window --config /tmp/bench.toml &     # bind 127.0.0.1:8444, audio.enabled = false
HOLD=20 cargo run -p streame-rtc --features client --example fake_phone -- 127.0.0.1 8444
curl -sk https://127.0.0.1:8444/api/state | jq .stats
```

`cargo test --workspace --all-features` couvre le protocole de signaling, les paramètres
H264, l'offre SDP webrtc-rs et le ré-échantillonneur audio.

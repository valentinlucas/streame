# Streame iOS — app native (Swift + Rust)

Application iPhone/iPad qui fait la même chose que la page web `https://<mac>:8443/` :
caméra + micro envoyés en WebRTC à la régie Mac, retour audio, badge « à l'antenne ». La
différence : **tout le média est en Rust, avec le même code que la régie** (crate
`crates/streame-rtc`) — webrtc-rs pour le transport, libopus pour l'audio, VideoToolbox pour
le H264. Pas de libwebrtc, pas de GStreamer. Sur l'iPhone, le micro et la sortie passent par
`AVAudioEngine` avec le traitement vocal Apple (annulation d'écho, gain automatique,
réduction de bruit — ce que fait libwebrtc lui-même) et le PCM est échangé avec Rust ; cpal
reste le backend du Mac et des bancs (feature `cpal-audio`).

```text
 iPhone (Swift)                          Rust (crates/streame-rtc, feature `client`)
 AVCaptureSession ─ CVPixelBuffer NV12 ─► thread h264-encode : réduction (VTPixelTransfer) ─► VideoToolbox ─► H264 Annex-B
 AVAudioEngine (traitement vocal) ─ PCM ─► anneau ─► libopus (20 ms, FEC, réveil par signal)   │
 NWBrowser (_streame._tcp) ─ hôte:port ─► WebSocket wss://<mac>/ws (certificat mémorisé, TOFU)  │
 SwiftUI (réglages, direct, boutons) ◄─── événements ordonnés (état, connecté, antenne, stats, certificat)
                                          webrtc-rs répondeur : Opus sendrecv + H264 sendonly ─► Mac
                                          GCC (TWCC) ─► débit + résolution VideoToolbox · PLI/FIR ─► image-clé
                                          Mac ─► Opus ─► libopus ─► anneau ─► PCM ─► AVAudioEngine
```

## Threads

| Thread / file | Rôle | Ne fait jamais |
| --- | --- | --- |
| Principal | SwiftUI, modèle `@Observable`, Bonjour, événements Rust (file principale, FIFO) | audio, vidéo, appel bloquant |
| `fr.lvlab.streame.session` (série) | `streame_client_start/stop`, démarrage/arrêt du moteur audio (1 à 3 s) | — |
| `fr.lvlab.streame.camera` (série) | délégué caméra, configuration, rotation de la connexion de sortie | appel bloquant (l'arrêt part sur la file de session) |
| `fr.lvlab.streame.audio` (série) | session audio, construction et **reconstruction** du graphe (route, interruption, services média) | — |
| E/S audio temps réel | copie PCM ↔ anneaux Rust (pas d'allocation, pas de verrou partagé, pas de journal) | — |
| `h264-encode` (Rust) | réduction de résolution + VideoToolbox | — |
| `streame-rtc` ×2 (Tokio) | signaling, RTP, Opus, GCC, statistiques | — |

## Prérequis (sur le Mac de build)

- Xcode 15+ (SDK iOS 17), `xcode-select -s /Applications/Xcode.app`
- Rust (`rustup`), cibles ajoutées automatiquement par le script (`aarch64-apple-ios`, `aarch64-apple-ios-sim`)
- `brew install cmake xcodegen` — cmake compile libopus depuis les sources pour l'iPhone
  (la libopus Homebrew du Mac n'est pas utilisée, `LIBOPUS_NO_PKG=1`)

## Compiler

```bash
cd ios
xcodegen generate            # crée Streame.xcodeproj à partir de project.yml
open Streame.xcodeproj       # choisir votre équipe de signature, puis Run sur l'iPhone
```

En ligne de commande, sans ouvrir Xcode (build signé, installation et lancement sur l'iPhone
branché ou jumelé en Wi-Fi) :

```bash
./ios/deploy.sh            # Debug, premier iPhone physique connecté
./ios/deploy.sh Release    # ou avec un UDID : ./ios/deploy.sh Debug 00008140-…
```

L'équipe de signature est dans `project.yml` (`DEVELOPMENT_TEAM`).

La phase de pré-build appelle `ios/build-rust.sh <plateforme> <configuration>`, qui produit
`ios/StreameCore/lib/{ios,ios-sim}/libstreame_ios.a` (crate `crates/streame-ios`, API C dans
`StreameCore/include/streame.h`). À la main : `./ios/build-rust.sh iphoneos Release`. Le Rust
est toujours compilé en `release`, même pour la configuration Debug de Xcode (17 Mo au lieu de
62 Mo, lancement et vidéo plus rapides) ; `STREAME_RUST_PROFILE=debug` pour déboguer le Rust.

Le simulateur n'a pas de caméra : il sert à tester l'interface et le signaling, pas la vidéo.
Il n'est compilé qu'en arm64 (Mac Apple Silicon) ; x86_64 est exclu du projet.

Vérification sans signature, en ligne de commande :

```bash
xcodebuild -project Streame.xcodeproj -scheme Streame -sdk iphoneos -destination 'generic/platform=iOS' CODE_SIGNING_ALLOWED=NO build
```

## Côté régie (Mac)

`streame` annonce le service Bonjour `_streame._tcp` (`server.mdns = true`, nom = `server.name`
ou le nom de la machine) : l'app liste les régies trouvées, sinon on saisit l'adresse. Le
certificat auto-signé de la régie est accepté tel quel par l'app (réseau local).

## Fichiers

| Fichier | Rôle |
| --- | --- |
| `Streame/StreameApp.swift` | Point d'entrée SwiftUI, verrouillage paysage, écran réglages/direct. |
| `Streame/StreameModel.swift` | État de l'app, réglages persistés, démarrage/arrêt du client Rust, événements. |
| `Streame/DeviceName.swift` | Nom affiché sur la régie : nom du téléphone, sinon nom commercial du modèle. |
| `Streame/CameraCapture.swift` | `AVCaptureSession` : objectifs, 1080p/720p/480p, NV12, 30 i/s, rotation paysage. |
| `Streame/AudioEngineIO.swift` | `AVAudioEngine` sur sa file : traitement vocal, `AVAudioSinkNode` (micro → Rust) et `AVAudioSourceNode` (Rust → sortie), graphe reconstruit sur changement de route, fin d'interruption ou réinitialisation des services média. |
| `Streame/AtomicFlag.h/.c` | Drapeau atomique C11 lu par les callbacks temps réel (coupe les callbacks avant l'arrêt). |
| `Streame/BonjourBrowser.swift` | Découverte des régies (`NWBrowser`) et résolution en IP. |
| `Streame/Views.swift` | Aperçu (`AVCaptureVideoPreviewLayer`), écran de réglages, écran du direct. |
| `StreameCore/include/streame.h` | API C de la bibliothèque Rust (`streame_client_start/push_video/push_audio/pull_audio/set_mic/set_speaker/stop`). |
| `build-rust.sh` | Compilation croisée de `crates/streame-ios` pour iPhone/simulateur. |
| `project.yml` | Projet Xcode (XcodeGen) : frameworks, bridging header, chemins des bibliothèques, Info.plist. |

## Limites / notes

- **Audio** : `AVAudioSession` en `playAndRecord` / `videoChat`, 48 kHz demandés (Rust
  ré-échantillonne si la session impose autre chose, par ex. 24 kHz avec le traitement vocal).
  AirPods **imposés en HFP** par défaut (micro et sortie sur l'oreillette, bande étroite) :
  seul HFP est déclaré à la session et l'entrée Bluetooth est sélectionnée comme entrée
  préférée, y compris quand les AirPods se connectent en cours de direct. Le réglage
  « AirPods : micro et son » désactivé passe en A2DP (sortie haute fidélité, micro de
  l'iPhone). Micro mono en Opus 64 kb/s.
- **Certificat de la régie** : auto-signé, donc mémorisé à la première connexion (empreinte
  SHA-256 par hôte, `UserDefaults`). Si la régie en présente un autre, la connexion est refusée
  et l'écran de réglages propose d'accepter le nouveau (ou d'oublier l'ancien).
- **Cycle de vie** : passage en arrière-plan = fin propre du direct (« bye ») ; erreurs
  d'exécution de la capture relancées ; interruption caméra affichée ; état thermique
  `serious`/`critical` = capture plafonnée à 720p jusqu'au retour à la normale.
- **ICE** : la bibliothèque ne se lie qu'aux interfaces `en*` (Wi-Fi/Ethernet), pas au
  cellulaire ni aux VPN, pour ne proposer au Mac que des candidats joignables.
- **Démarrage** : `streame_client_start` tourne hors du thread principal ; l'écran du direct
  s'affiche immédiatement et l'état de connexion s'y met à jour.
- **480p** = préréglage `vga640x480` (4:3), 720p/1080p sont en 16:9.
- **Contrôle de congestion** : GCC de webrtc-rs (feedback TWCC de la régie), débit de départ
  annoncé par la régie (`x-google-start-bitrate`), plafond `video_max_bitrate_kbps` (part
  proportionnelle en 720p/480p, comme la page). Sous 1,5 Mb/s la résolution descend (720p,
  540p, 360p) après 2 s, et remonte après 5 s avec 30 % de marge, comme libwebrtc ; la file
  d'images encodées est bornée (3) et une image jetée force une image-clé.
- Testé sur Mac avec `cargo run -p streame-rtc --features client --example fake_phone` (même
  code, caméra et micro synthétiques) ; l'app elle-même doit être compilée avec Xcode.

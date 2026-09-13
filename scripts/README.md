# Outils de développement

## `fake-phone.mjs` — faux téléphone pour tester sans iPhone

Lance un Chromium avec une **caméra et un micro synthétiques** sur la page « téléphone » de
streame, en suivant le même parcours que l'appareil réel (getUserMedia + WebRTC via `web/app.js`).
Pratique pour reproduire les problèmes d'ICE, de signaling, de jitter buffer et de reconnexion
sans matériel, en boucle.

> L'offre ne propose que H264 : Chromium négocie en **H264** (encodeur logiciel) et le banc exerce
> donc le même chemin H264 → VideoToolbox → IOSurface que l'iPhone.

### Installation (une fois)

```bash
cd scripts
npm init -y
npm i playwright        # réutilise le Chromium déjà en cache si présent
```

### Utilisation

1. Lancer streame sur un port de test, avec une carte son par défaut (sans la Wing) :

   ```bash
   # copier la config en changeant le port et les périphériques audio, puis :
   RUST_LOG=info,streame=debug ./target/debug/streame --no-window --config test.toml
   ```

2. Lancer le faux téléphone :

   ```bash
   URL=https://127.0.0.1:8444/ ITER=3 HOLD=8000 node scripts/fake-phone.mjs
   ```

   - `ITER` : nombre de connexions successives (pour reproduire une intermittence)
   - `HOLD` : durée de maintien de chaque connexion (ms)
   - `HEADLESS=0` : afficher la fenêtre du navigateur

3. Observer le log de streame : `piste Video « video/h264 »`, `décodage VideoToolbox (matériel,
   IOSurface) démarré`, `décodage Opus (libopus) démarré`, `images soumises`, et pour les fichiers
   d'habillage `vidéo d'habillage « … »` puis `passage terminé (N images, 0 sautées)` à chaque
   boucle. Une vidéo de test se fabrique avec ffmpeg :
   `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=25 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 6 -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 assets/overlay.mp4`.

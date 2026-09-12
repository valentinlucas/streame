# Outils de développement

## `fake-phone.mjs` — faux téléphone pour tester sans iPhone

Lance un Chromium avec une **caméra et un micro synthétiques** sur la page « téléphone » de
streame, en suivant le même parcours que l'appareil réel (getUserMedia + WebRTC via `web/app.js`).
Pratique pour reproduire les problèmes d'ICE, de signaling, de jitter buffer et de reconnexion
sans matériel, en boucle.

> Depuis que l'offre met H264 en tête, Chromium négocie lui aussi en **H264** (encodeur logiciel) :
> le banc exerce donc le même chemin H264 → `vtdec` que l'iPhone. Mettre `video_codec = "VP8"`
> dans la config de test pour exercer le chemin VP8.

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

3. Observer le log de streame : `chaîne de décodage`, `decodebin : pad ajouté`, `flux … connecté`,
   `paquets RTP poussés`, et surtout d'éventuelles lignes `pipeline decode-… : <erreur>`.

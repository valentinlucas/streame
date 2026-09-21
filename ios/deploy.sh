#!/bin/bash
# Compile l'app (Rust + Swift, signée) et l'installe puis la lance sur l'iPhone, sans ouvrir
# Xcode. Usage : ./ios/deploy.sh [Debug|Release] [UDID]
#   UDID : identifiant de l'appareil (`xcrun devicectl list devices`) ; par défaut le premier
#   iPhone physique connecté (USB ou Wi-Fi jumelé).
set -euo pipefail
cd "$(dirname "$0")"
CONFIG="${1:-Debug}"
# `|| true` : sans iPhone connecté, grep échoue et `set -e -o pipefail` quitterait ici sans un mot.
LIST="$(xcrun devicectl list devices 2>/dev/null || true)"
CONNECTED="$(printf '%s\n' "$LIST" | grep -E 'physical' | grep -E 'connected' || true)"
if [ -z "${2:-}" ] && [ "$(printf '%s\n' "$CONNECTED" | grep -c .)" -gt 1 ]; then
  {
    echo "plusieurs iPhone connectés : préciser l'UDID en deuxième argument (./ios/deploy.sh Debug <UDID>)"
    printf '%s\n' "$CONNECTED"
  } >&2
  exit 1
fi
UDID="${2:-$(printf '%s\n' "$CONNECTED" | grep -oE '[0-9A-F]{8}-[0-9A-F]{16}' | head -1 || true)}"
if [ -z "$UDID" ]; then
  {
    echo "aucun iPhone connecté. Appareils vus par devicectl :"
    printf '%s\n' "$LIST"
    echo "Un iPhone jumelé en Wi-Fi mais « available » n'est pas joignable : le déverrouiller,"
    echo "le brancher en USB, ou passer son UDID en deuxième argument."
  } >&2
  exit 1
fi
BUNDLE_ID="fr.lvlab.streame"
DERIVED="$PWD/build"

[ -d Streame.xcodeproj ] || xcodegen generate
echo "→ build $CONFIG pour $UDID"
xcodebuild -project Streame.xcodeproj -scheme Streame -configuration "$CONFIG" \
  -destination "id=$UDID" -derivedDataPath "$DERIVED" -allowProvisioningUpdates build \
  | grep -E 'error:|warning: .*Streame/|\*\* BUILD' || true
APP="$DERIVED/Build/Products/$CONFIG-iphoneos/Streame.app"
[ -d "$APP" ] || { echo "build échoué : $APP absent" >&2; exit 1; }

echo "→ installation"
xcrun devicectl device install app --device "$UDID" "$APP"
echo "→ lancement"
xcrun devicectl device process launch --device "$UDID" --activate "$BUNDLE_ID"

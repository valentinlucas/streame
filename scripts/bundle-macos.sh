#!/bin/zsh
# Construit Streame.app (bundle macOS) à partir du binaire release, avec l'icône
# assets/icon/streame.svg rendue en .icns, et l'installe dans /Applications si demandé.
#
#   scripts/bundle-macos.sh            # → target/bundle/Streame.app
#   scripts/bundle-macos.sh --install  # + copie dans /Applications (remplace l'ancienne)
#
# Outils : cargo, rsvg-convert (brew install librsvg), sips, iconutil, codesign (Xcode CLT).
set -euo pipefail
cd "$(dirname "$0")/.."

NAME=Streame
ID=fr.lvlab.streame
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
OUT=target/bundle
APP=$OUT/$NAME.app
WORK=target/bundle/work

echo "▸ cargo build --release"
cargo build --release

echo "▸ icône"
rm -rf "$WORK"; mkdir -p "$WORK/$NAME.iconset"
rsvg-convert -w 1024 -h 1024 assets/icon/streame.svg -o "$WORK/icon-1024.png"
for s in 16 32 128 256 512; do
  sips -z $s $s "$WORK/icon-1024.png" --out "$WORK/$NAME.iconset/icon_${s}x${s}.png" >/dev/null
  d=$((s*2))
  sips -z $d $d "$WORK/icon-1024.png" --out "$WORK/$NAME.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$WORK/$NAME.iconset" -o "$WORK/$NAME.icns"

echo "▸ bundle $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/streame "$APP/Contents/MacOS/streame-bin"
cp "$WORK/$NAME.icns" "$APP/Contents/Resources/$NAME.icns"

# Lanceur : depuis le Finder il n'y a ni terminal ni dossier courant utile ; on journalise dans
# ~/Library/Logs/streame.log (le journal précédent est conservé en streame.previous.log) et le
# binaire trouve sa configuration dans ~/Library/Application Support/streame/.
cat > "$APP/Contents/MacOS/streame" <<'SH'
#!/bin/zsh
DIR="$(cd "$(dirname "$0")" && pwd)"
LOG="$HOME/Library/Logs/streame.log"
mkdir -p "$HOME/Library/Logs"
[ -f "$LOG" ] && mv -f "$LOG" "$HOME/Library/Logs/streame.previous.log"
cd "$HOME"
exec "$DIR/streame-bin" "$@" >>"$LOG" 2>&1
SH
chmod +x "$APP/Contents/MacOS/streame" "$APP/Contents/MacOS/streame-bin"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>$ID</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>streame</string>
  <key>CFBundleIconFile</key><string>$NAME</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>LVLab — MIT</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Streame capte la carte son (retour et routage audio) configurée dans streame.toml.</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>Streame reçoit la caméra du téléphone (WebRTC), l'annonce en Bonjour et écoute l'OSC sur le réseau local.</string>
  <key>NSBonjourServices</key>
  <array><string>_streame._tcp</string></array>
</dict>
</plist>
PLIST

echo "▸ signature ad hoc"
codesign --force --deep --sign - "$APP"
codesign --verify --deep "$APP" && echo "  ok"

if [[ "${1:-}" == "--install" ]]; then
  DEST=/Applications/$NAME.app
  echo "▸ installation → $DEST"
  rm -rf "$DEST"
  cp -R "$APP" "$DEST"
  # Rafraîchit l'icône et l'enregistrement LaunchServices.
  touch "$DEST"
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$DEST" >/dev/null 2>&1 || true
  echo "  installé. Configuration : ~/Library/Application Support/streame/streame.toml"
  echo "  journal : ~/Library/Logs/streame.log"
fi

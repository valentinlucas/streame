#!/bin/bash
# Compile la bibliothèque Rust (crates/streame-ios → libstreame_ios.a) pour l'iPhone ou le
# simulateur. Appelé par Xcode (phase de pré-build) ou à la main :
#   ./ios/build-rust.sh iphoneos Release
#   ./ios/build-rust.sh iphonesimulator Debug
# Prérequis : Xcode, rustup, cmake (brew install cmake) — libopus est compilée depuis les
# sources pour la cible (la libopus Homebrew du Mac ne sert pas).
set -euo pipefail
PLATFORM="${1:-iphoneos}"
CONFIG="${2:-Release}"
cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
# Xcode exporte SDKROOT (SDK iPhone) : les build scripts des crates (compilés pour le Mac) ne
# doivent pas le voir. cc/cmake retrouvent le bon SDK par la cible.
unset SDKROOT
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}"
# libopus : pas de pkg-config (viserait la version Mac), compilation statique depuis les sources.
export LIBOPUS_NO_PKG=1 OPUS_STATIC=1
# La libopus embarquée par audiopus_sys date d'avant CMake 3.5 ; CMake 4 la refuse sans ceci.
export CMAKE_POLICY_VERSION_MINIMUM=3.5
# bindgen (coreaudio-sys) : libclang de Xcode si non précisé.
if [ -z "${LIBCLANG_PATH:-}" ]; then
  XC="$(xcode-select -p 2>/dev/null || true)"
  if [ -d "$XC/Toolchains/XcodeDefault.xctoolchain/usr/lib" ]; then
    export LIBCLANG_PATH="$XC/Toolchains/XcodeDefault.xctoolchain/usr/lib"
  fi
fi

case "$PLATFORM" in
  iphoneos) TARGET=aarch64-apple-ios; OUT=ios ;;
  iphonesimulator) TARGET=aarch64-apple-ios-sim; OUT=ios-sim ;;
  *) echo "plateforme inconnue : $PLATFORM (iphoneos | iphonesimulator)" >&2; exit 1 ;;
esac
# Rust toujours optimisé, même pour la configuration Debug de Xcode : non optimisée, la
# bibliothèque pèse 60 Mo et ralentit le lancement et la vidéo. STREAME_RUST_PROFILE=debug pour
# déboguer le Rust lui-même.
if [ "${STREAME_RUST_PROFILE:-release}" = "debug" ]; then PROFILE=debug; FLAG=""; else PROFILE=release; FLAG="--release"; fi
echo "→ Rust $PROFILE (configuration Xcode $CONFIG)"

rustup target add "$TARGET" >/dev/null
cargo build -p streame-ios --target "$TARGET" $FLAG
mkdir -p "ios/StreameCore/lib/$OUT"
cp "target/$TARGET/$PROFILE/libstreame_ios.a" "ios/StreameCore/lib/$OUT/libstreame_ios.a"
date > "ios/StreameCore/lib/$PLATFORM-stamp"
echo "→ ios/StreameCore/lib/$OUT/libstreame_ios.a ($TARGET, $PROFILE)"

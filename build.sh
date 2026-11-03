#!/bin/bash
# Builds "Mac Excel.app" as a universal binary (arm64 + x86_64), ad-hoc signed, into
# $HOME/Library/Caches/MacExcelBuild. Usage: ./build.sh [debug|release]  (default: debug)
set -euo pipefail
cd "$(dirname "$0")"

APP_NAME="MacExcel"
BUNDLE_NAME="Mac Excel"
BUNDLE_ID="com.manikpruthi.mac-excel"
BUILD_DIR="${MAC_EXCEL_BUILD_DIR:-$HOME/Library/Caches/MacExcelBuild}"
APP="$BUILD_DIR/$BUNDLE_NAME.app"
MODE="${1:-debug}"

if [ "$MODE" = "release" ] || [ "$MODE" = "--release" ]; then
    PROFILE_ARGS=(--release)
    PROFILE_DIR="release"
else
    PROFILE_ARGS=()
    PROFILE_DIR="debug"
fi

VERSION="$(awk '/^\[/ { p = ($0 == "[workspace.package]"); next } p && $1 == "version" { gsub(/[" ]/, "", $3); print $3; exit }' Cargo.toml)"
SHORT_VERSION="${VERSION%%-*}"
export MACOSX_DEPLOYMENT_TARGET=11.0
export GRIDCRAFT_BUILD_SHA="${GRIDCRAFT_BUILD_SHA:-$(git rev-parse HEAD 2>/dev/null || echo unknown)}"
export GRIDCRAFT_BUILD_DATE="${GRIDCRAFT_BUILD_DATE:-$(date -u +%Y-%m-%d)}"

TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
for t in "${TARGETS[@]}"; do
    rustup target list --installed | grep -qx "$t" || rustup target add "$t"
done

# One cargo invocation for both targets: cargo schedules them together.
echo "Compiling $BUNDLE_NAME $VERSION for ${TARGETS[*]} ($MODE)…"
TARGET_ARGS=()
for t in "${TARGETS[@]}"; do TARGET_ARGS+=(--target "$t"); done
cargo build ${PROFILE_ARGS[@]+"${PROFILE_ARGS[@]}"} -p gridcraft -p gridcraft-cli "${TARGET_ARGS[@]}"

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create "$TARGET_DIR/aarch64-apple-darwin/$PROFILE_DIR/gridcraft" "$TARGET_DIR/x86_64-apple-darwin/$PROFILE_DIR/gridcraft" \
    -output "$APP/Contents/MacOS/$APP_NAME"
lipo -create "$TARGET_DIR/aarch64-apple-darwin/$PROFILE_DIR/gridcraft-cli" "$TARGET_DIR/x86_64-apple-darwin/$PROFILE_DIR/gridcraft-cli" \
    -output "$BUILD_DIR/mac-excel-cli"
lipo -info "$APP/Contents/MacOS/$APP_NAME"

cp assets/app-icon/gridcraft.icns "$APP/Contents/Resources/$APP_NAME.icns"
cp NOTICE ATTRIBUTION.md "$APP/Contents/Resources/"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@SHORT_VERSION@/$SHORT_VERSION/g" -e "s/@BUILD_SHA@/$GRIDCRAFT_BUILD_SHA/g" \
    -e "s#<string>GridCraft</string>#<string>$BUNDLE_NAME</string>#g" \
    -e "s#<string>ai.storyteller.gridcraft</string>#<string>$BUNDLE_ID</string>#" \
    packaging/macos/Info.plist.in >"$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleExecutable $APP_NAME" -c "Set :CFBundleIconFile $APP_NAME" "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
printf 'APPL????' >"$APP/Contents/PkgInfo"

codesign --force --sign - --options runtime --entitlements packaging/macos/entitlements.plist \
    --requirements "=designated => identifier \"$BUNDLE_ID\"" "$APP"
codesign --force --sign - "$BUILD_DIR/mac-excel-cli"
codesign --verify --strict "$APP"

touch "$BUILD_DIR/.metadata_never_index"
echo "Built $APP"

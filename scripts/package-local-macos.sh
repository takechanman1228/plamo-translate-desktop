#!/bin/sh
# Build a separate, unpublished app for local trials. No remote operations.
set -eu
cd "$(dirname "$0")/.."
test "$(uname -s)" = Darwin
./scripts/local-dev.sh build -p plamo-translate-desktop --release --locked
app='dist/PLaMo Translate Local.app'
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/plamo-translate-desktop "$app/Contents/MacOS/plamo-translate-desktop.new"
mv -f "$app/Contents/MacOS/plamo-translate-desktop.new" "$app/Contents/MacOS/plamo-translate-desktop"
python3 - "$app/Contents/Info.plist" <<'PY'
import plistlib, sys
with open(sys.argv[1], 'wb') as f:
    plistlib.dump({
        'CFBundleDevelopmentRegion': 'en',
        'CFBundleDisplayName': 'PLaMo Translate Local',
        'CFBundleName': 'PLaMo Translate Local',
        'CFBundleExecutable': 'plamo-translate-desktop',
        'CFBundleIdentifier': 'local.plamo-translate-desktop.preview',
        'CFBundleInfoDictionaryVersion': '6.0',
        'CFBundlePackageType': 'APPL',
        'CFBundleShortVersionString': '0.6.0',
        'CFBundleVersion': '0.6.0',
        'NSHighResolutionCapable': True,
        'NSPrincipalClass': 'NSApplication',
        'LSMinimumSystemVersion': '13.3',
        'LSEnvironment': {'PLAMO_LOCAL_PREVIEW': '1', 'PLAMO_LANG': 'ja'},
    }, f)
PY
if [ -n "${PLAMO_LOCAL_PDFIUM_DIR:-}" ]; then
    mkdir -p "$app/Contents/Resources/pdfium-licenses"
    cp "$PLAMO_LOCAL_PDFIUM_DIR/lib/libpdfium.dylib" "$app/Contents/Resources/libpdfium.dylib"
    cp "$PLAMO_LOCAL_PDFIUM_DIR"/licenses/* "$app/Contents/Resources/pdfium-licenses/"
    codesign --force --sign - "$app/Contents/Resources/libpdfium.dylib"
fi
if [ -n "${PLAMO_LOCAL_IPAFONT_DIR:-}" ]; then
    mkdir -p "$app/Contents/Resources/ipafont"
    cp "$PLAMO_LOCAL_IPAFONT_DIR/ipag.ttf" "$app/Contents/Resources/ipafont/ipag.ttf"
    cp "$PLAMO_LOCAL_IPAFONT_DIR/IPA_Font_License_Agreement_v1.0.txt" "$app/Contents/Resources/ipafont/"
    cp "$PLAMO_LOCAL_IPAFONT_DIR/Readme_IPAfont00303.txt" "$app/Contents/Resources/ipafont/"
fi
codesign --force --sign - "$app"
printf 'Local app: %s/%s\n' "$PWD" "$app"

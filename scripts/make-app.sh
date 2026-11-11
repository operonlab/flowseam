#!/bin/bash
# Build Flowseam.app for Apple silicon and Intel in one bundle, sign it, zip it.
#
#   scripts/make-app.sh <version>   ->  dist/Flowseam.app, dist/Flowseam-<version>-macos.zip
#
# macOS ties Accessibility and Input Monitoring grants to the bundle id and the signing certificate, not to
# one build. Every release is signed with the same certificate, so a user grants once and updates keep it.
# An ad-hoc signature would pin the grant to this one binary and ask again after every update.
#
# Needs: a rustup toolchain with the aarch64- and x86_64-apple-darwin targets (FLOWSEAM_TOOLCHAIN, holding
# rustup/ and cargo/), and the signing identity in the keychain (FLOWSEAM_SIGN_IDENTITY).
set -u

write_login_plist() {
  mkdir -p "$APP/Contents/Library/LaunchAgents" || fail "could not create LaunchAgents directory"
  local plist="$APP/Contents/Library/LaunchAgents/$BUNDLE_ID.login.plist"
  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$BUNDLE_ID.login</string>
  <key>BundleProgram</key><string>Contents/MacOS/flowseam</string>
  <key>ProgramArguments</key><array><string>flowseam</string><string>launch-at-login</string></array>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>AssociatedBundleIdentifiers</key><array><string>$BUNDLE_ID</string></array>
</dict>
</plist>
EOF
  plutil -lint "$plist" || fail "invalid login agent plist"
}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TOOLS=${FLOWSEAM_TOOLCHAIN:-$HOME/.local/share/flowseam-release}
IDENTITY=${FLOWSEAM_SIGN_IDENTITY:-Flowseam Release}
BUNDLE_ID=com.operonlab.flowseam
TARGET_DIR=${CARGO_TARGET_DIR:-$TOOLS/target}
DIST=$ROOT/dist
APP=$DIST/Flowseam.app

fail() { echo "make-app: $*" >&2; exit 1; }

if [ "${1:-}" = "--write-login-plist-for-test" ]; then
  APP=${2:?usage: scripts/make-app.sh --write-login-plist-for-test <app path>}
  write_login_plist
  exit 0
fi
VER=${1:?usage: scripts/make-app.sh <version>}

CARGO=$TOOLS/cargo/bin/cargo
[ -x "$CARGO" ] || fail "no toolchain at $TOOLS (rustup with aarch64- and x86_64-apple-darwin)"
export RUSTUP_HOME=$TOOLS/rustup CARGO_HOME=$TOOLS/cargo

crate_ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/flowseam/Cargo.toml" | head -1)
[ "$crate_ver" = "$VER" ] || fail "crates/flowseam is $crate_ver, not $VER: bump the crate versions first"

mkdir -p "$TARGET_DIR"
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  log=$TARGET_DIR/build-$t.log
  if ! (cd "$ROOT" && CARGO_TARGET_DIR=$TARGET_DIR "$CARGO" build --release --locked -p flowseam --target "$t") > "$log" 2>&1; then
    tail -20 "$log" >&2
    fail "build for $t failed (log: $log)"
  fi
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/assets/icon/AppIcon.icns" "$APP/Contents/Resources/" || fail "no assets/icon/AppIcon.icns"
# The dependencies' licenses ship inside the app. about.toml lists the accepted ones; any other fails the build.
ABOUT=$(command -v cargo-about) || fail "cargo-about is needed for the license notices: cargo install cargo-about --locked --features cli"
if ! (cd "$ROOT" && "$ABOUT" about generate about.hbs -o "$APP/Contents/Resources/THIRD-PARTY-NOTICES.html") > "$TARGET_DIR/about.log" 2>&1; then
  tail -20 "$TARGET_DIR/about.log" >&2
  fail "license notices failed (log: $TARGET_DIR/about.log)"
fi
lipo -create -output "$APP/Contents/MacOS/flowseam" \
  "$TARGET_DIR/aarch64-apple-darwin/release/flowseam" \
  "$TARGET_DIR/x86_64-apple-darwin/release/flowseam" || fail "lipo failed"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>Flowseam</string>
  <key>CFBundleDisplayName</key><string>Flowseam</string>
  <key>CFBundleExecutable</key><string>flowseam</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>CFBundleVersion</key><string>$VER</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

write_login_plist
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP" || fail "signing with \"$IDENTITY\" failed"
codesign --verify --strict "$APP" || fail "the signature does not verify"
# The designated requirement must pin the certificate; a cdhash-only requirement would not carry grants over.
req=$(codesign -d -r- "$APP" 2>&1)
case "$req" in
  *"identifier \"$BUNDLE_ID\""*"certificate leaf = H"*) ;;
  *) fail "unexpected designated requirement: $req" ;;
esac
for arch in arm64 x86_64; do
  lipo "$APP/Contents/MacOS/flowseam" -verify_arch "$arch" || fail "no $arch slice"
done

ZIP=$DIST/Flowseam-$VER-macos.zip
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP" || fail "zip failed"
shasum -a 256 "$ZIP" | awk '{print $1}' > "$ZIP.sha256"
echo "built $ZIP"
echo "sha256 $(cat "$ZIP.sha256")"

#!/bin/bash
# Publish one Flowseam release: the public source, the signed macOS app and the Windows exe on GitHub
# Releases, and the Homebrew cask.
#
#   RELEASE_CHECK_DENY='<private names, as an extended regex>' WINDOWS_EXE=<flowseam.exe> \
#     scripts/release.sh <version> <notes.md> [ref]
#
# Builds from a clean export of <ref> (default: local-main), never from a working tree. Nothing leaves this
# machine until the private-name scan, the tests, the build and the signature check have all passed.
# The private names come from the environment so they are never written into a public file.
# RELEASE_TRAILER, if set, is appended to the public commit messages.
#
# The Windows exe is built on a Windows PC from the same <ref> (cargo build --release --locked -p flowseam)
# and tested there; this script only checks and uploads it. <exe>.commit must hold the full commit id it was
# built from, and that must be <ref>. WINDOWS_EXE=none publishes without it.
set -u

VER=${1:?usage: scripts/release.sh <version> <notes.md> [ref]}
NOTES=${2:?usage: scripts/release.sh <version> <notes.md> [ref]}
REF=${3:-local-main}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
REPO=operonlab/flowseam
TAP=operonlab/homebrew-tap

fail() { echo "release: $*" >&2; exit 1; }

[ -n "${RELEASE_CHECK_DENY:-}" ] || fail "set RELEASE_CHECK_DENY to the private names to scan for"
[ -f "$NOTES" ] || fail "no notes file $NOTES"
case "$VER" in [0-9]*.[0-9]*.[0-9]*) ;; *) fail "version must look like 0.1.0" ;; esac
gh release view "v$VER" --repo "$REPO" > /dev/null 2>&1 && fail "v$VER is already released"

WIN_EXE=${WINDOWS_EXE:?set WINDOWS_EXE to the Windows flowseam.exe built from $REF, or to none}
if [ "$WIN_EXE" != none ]; then
  [ -f "$WIN_EXE" ] || fail "no Windows exe at $WIN_EXE"
  [ "$(head -c 2 "$WIN_EXE")" = MZ ] || fail "$WIN_EXE is not a Windows executable"
  want=$(git -C "$ROOT" rev-parse --verify "$REF^{commit}") || fail "cannot resolve $REF"
  built=$(cat "$WIN_EXE.commit" 2>/dev/null)
  [ "$built" = "$want" ] || fail "$WIN_EXE was built from '${built:-?}', not $REF ($want); see $WIN_EXE.commit"
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/flowseam-release.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/src"
git -C "$ROOT" archive "$REF" | tar -x -C "$WORK/src" || fail "cannot export $REF"

# The scan reads a copy with every Unicode hyphen and dash folded to "-": a name typed with a non-breaking
# hyphen (U+2011) does not match an ASCII pattern, and three such names once passed this scan unseen.
fold_dashes() { perl -CSD -pe 's/\p{Dash}/-/g'; }
[ "$(printf 'a\342\200\221b c\342\270\272d' | fold_dashes)" = "a-b c-d" ] || fail "dash folding does not work here"
cp -R "$WORK/src" "$WORK/scan"
grep -rIl --exclude=Cargo.lock . "$WORK/scan" | while IFS= read -r f; do
  fold_dashes < "$f" > "$f.folded" && mv "$f.folded" "$f"
done
if hits=$(grep -rIEn "$RELEASE_CHECK_DENY" "$WORK/scan" --exclude=Cargo.lock); then
  echo "$hits" | sed "s|$WORK/scan/||" | head -20 >&2
  fail "private names in $REF"
fi
fold_dashes < "$NOTES" | grep -qIE "$RELEASE_CHECK_DENY" && fail "private names in $NOTES"

echo "release: testing $REF"
(cd "$WORK/src" && cargo test --workspace --locked) > "$WORK/test.log" 2>&1 || { tail -40 "$WORK/test.log" >&2; fail "tests failed"; }

echo "release: building the app"
(cd "$WORK/src" && scripts/make-app.sh "$VER") || fail "packaging failed"
ZIP=$WORK/src/dist/Flowseam-$VER-macos.zip
SHA=$(cat "$ZIP.sha256")

msg() { if [ -n "${RELEASE_TRAILER:-}" ]; then printf '%s\n\n%s\n' "$1" "$RELEASE_TRAILER"; else printf '%s\n' "$1"; fi; }

echo "release: publishing the source"
gh repo clone "$REPO" "$WORK/pub" -- -q || fail "cannot clone $REPO"
rsync -a --delete --exclude .git --exclude dist "$WORK/src/" "$WORK/pub/" || fail "copy failed"
git -C "$WORK/pub" add -A
git -C "$WORK/pub" commit -q -m "$(msg "Flowseam $VER")" || fail "nothing changed since the last release"
git -C "$WORK/pub" tag "v$VER"
git -C "$WORK/pub" push -q origin main "v$VER" || fail "push to $REPO failed"

echo "release: uploading the app"
ASSETS=("$ZIP")
if [ "$WIN_EXE" != none ]; then
  EXE=$WORK/Flowseam-$VER-windows-x64.exe
  cp "$WIN_EXE" "$EXE" || fail "cannot copy $WIN_EXE"
  ASSETS+=("$EXE")
fi
gh release create "v$VER" "${ASSETS[@]}" --repo "$REPO" --title "Flowseam $VER" --notes-file "$NOTES" || fail "GitHub release failed"

echo "release: updating the cask"
gh repo clone "$TAP" "$WORK/tap" -- -q || fail "cannot clone $TAP"
mkdir -p "$WORK/tap/Casks"
cat > "$WORK/tap/Casks/flowseam.rb" <<EOF
cask "flowseam" do
  version "$VER"
  sha256 "$SHA"

  url "https://github.com/$REPO/releases/download/v#{version}/Flowseam-#{version}-macos.zip"
  name "Flowseam"
  desc "Share one keyboard, mouse, clipboard and files between Macs and Windows PCs"
  homepage "https://github.com/$REPO"

  depends_on macos: :ventura

  app "Flowseam.app"

  # Signed with Flowseam's own certificate but not notarized: without this, Gatekeeper refuses to open the
  # downloaded app. The signature stays, and it is what keeps the privacy grants across updates.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/Flowseam.app"]
  end

  uninstall quit: "com.operonlab.flowseam"

  caveats <<~EOS
    Open Flowseam once and grant Accessibility and Input Monitoring in
    System Settings > Privacy & Security. Updates keep these grants.
    Every machine must run the same Flowseam version.
  EOS
end
EOF
git -C "$WORK/tap" add Casks/flowseam.rb
git -C "$WORK/tap" commit -q -m "$(msg "flowseam $VER")" || fail "cask unchanged"
git -C "$WORK/tap" push -q origin HEAD || fail "push to $TAP failed"

echo "release: v$VER is out"
echo "  brew update && brew upgrade --cask flowseam   (first time: brew install --cask operonlab/tap/flowseam)"

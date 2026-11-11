#!/bin/bash
# Publish one Flowseam release: the public source, the signed macOS app and the Windows exe on GitHub
# Releases, and the Homebrew cask.
#
#   RELEASE_CHECK_DENY='<private names, as an extended regex>' WINDOWS_EXE=<flowseam.exe> \
#     scripts/release.sh <version> <notes.md> [ref]
#
# Builds from a clean export of <ref> (default: local-main), never from a working tree. Nothing leaves this
# machine until the private-name scan, the changelog check, the tests, the build and the signature check have
# all passed. The private names come from the environment so they are never written into a public file.
# RELEASE_TRAILER, if set, is appended to the public commit messages.
#
# The Windows exe is built on a Windows PC from the same <ref> (cargo build --release --locked -p flowseam)
# and tested there; this script only checks and uploads it. <exe>.commit must hold the full commit id it was
# built from, and that must be <ref>. WINDOWS_EXE=none publishes without it.
#
# The files go up as a draft, which is published only once GitHub holds exactly the files built here, with
# SHA256SUMS beside them. The cask moves to the new version only once the app's public download URL serves
# those bytes. A failure after the source is pushed keeps the work folder; once the cause is dealt with,
#
#   scripts/release.sh --resume <work folder>
#
# picks up from whatever state GitHub is in.
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
REPO=operonlab/flowseam
TAP=operonlab/homebrew-tap

fail() { echo "release: $*" >&2; exit 1; }
. "$ROOT/scripts/release-lib.sh" || fail "cannot read scripts/release-lib.sh"

[ -n "${RELEASE_CHECK_DENY:-}" ] || fail "set RELEASE_CHECK_DENY to the private names to scan for"
# An invalid pattern makes grep exit 2, which must not read as "no private names".
printf '' | grep -E -e "$RELEASE_CHECK_DENY" > /dev/null 2>&1
[ $? -eq 2 ] && fail "RELEASE_CHECK_DENY is not a valid extended regex"
# The scan reads a copy with every Unicode hyphen and dash folded to "-": a name typed with a non-breaking
# hyphen (U+2011) does not match an ASCII pattern, and three such names once passed this scan unseen.
fold_dashes() { perl -CSD -pe 's/\p{Dash}/-/g'; }
[ "$(printf 'a\342\200\221b c\342\270\272d' | fold_dashes)" = "a-b c-d" ] || fail "dash folding does not work here"
# scan_notes <file>: the release notes, folded, against the private names.
scan_notes() {
  fold_dashes < "$1" > "$WORK/notes.folded" || fail "could not fold the dashes in $1"
  grep -qIE "$RELEASE_CHECK_DENY" "$WORK/notes.folded"
  case $? in
    0) fail "private names in $1" ;;
    1) ;;
    *) fail "the private-name scan of $1 did not finish" ;;
  esac
}

# After the source is pushed a failure keeps the work folder, so that --resume can go on from it.
stuck() {
  trap - EXIT
  {
    echo "release: stopped: $1"
    echo "  the source and the tag v$VER are public; the work folder is kept at $WORK"
    echo "  to go on, deal with that, then: scripts/release.sh --resume $WORK"
  } >&2
  exit 1
}

# Everything after the source push. Each step looks at what GitHub already has, so it is safe to run again.
finish_release() {
  local why tagged
  # The tag on GitHub must be the one this run pushed; a v$VER left from before would carry these files.
  tagged=$(gh api "repos/$REPO/commits/refs/tags/v$VER" --jq .sha) || stuck "cannot ask $REPO which commit v$VER names"
  [ "$tagged" = "$COMMIT" ] || stuck "$REPO's v$VER names $tagged, not $COMMIT, the commit this release pushed"
  why=$(publish_checked "$REPO" "v$VER" "Flowseam $VER" "$WORK/notes.md" "${ASSETS[@]}") || stuck "$why"
  echo "release: waiting for the app's download"
  why=$(wait_for_download "$URL" "$SHA" 40 15)
  case $? in
    0) ;;
    2) stuck "$why; the cask must not point there until it serves the zip built here" ;;
    *) stuck "$why" ;;
  esac
  echo "release: updating the cask"
  git -C "$WORK/tap" push -q origin HEAD || stuck "push to $TAP failed"
  echo "release: v$VER is out"
  echo "  brew update && brew upgrade --cask flowseam   (first time: brew install --cask operonlab/tap/flowseam)"
}

if [ "${1:-}" = --resume ]; then
  WORK=${2:?usage: scripts/release.sh --resume <work folder>}
  [ -f "$WORK/state" ] && [ -d "$WORK/assets" ] && [ -d "$WORK/tap" ] || fail "$WORK is not a kept release folder"
  VER=$(sed -n 's/^VER=//p' "$WORK/state")
  SHA=$(sed -n 's/^SHA=//p' "$WORK/state")
  COMMIT=$(sed -n 's/^COMMIT=//p' "$WORK/state")
  [ -n "$VER" ] && [ -n "$SHA" ] && [ -n "$COMMIT" ] || fail "$WORK/state is incomplete"
  URL=https://github.com/$REPO/releases/download/v$VER/Flowseam-$VER-macos.zip
  # The folder must still hold what this run built: the zip state names, the files SHA256SUMS lists and no
  # others, and a committed cask for that zip. The notes are scanned again.
  zip=$WORK/assets/Flowseam-$VER-macos.zip
  [ -f "$zip" ] && [ "$(shasum -a 256 "$zip" | awk '{ print $1 }')" = "$SHA" ] || fail "$WORK/assets does not hold the zip $WORK/state names"
  (cd "$WORK/assets" && shasum -a 256 -c SHA256SUMS > /dev/null) || fail "$WORK/assets does not match its SHA256SUMS"
  listed=$(awk '{ print $2 }' "$WORK/assets/SHA256SUMS" | sort | tr '\n' ' ')
  present=$(cd "$WORK/assets" && for f in *; do [ "$f" = SHA256SUMS ] || echo "$f"; done | sort | tr '\n' ' ')
  [ "$listed" = "$present" ] || fail "$WORK/assets holds ${present}but its SHA256SUMS lists $listed"
  cask=$(git -C "$WORK/tap" show HEAD:Casks/flowseam.rb) || fail "$WORK/tap has no committed cask"
  { printf '%s\n' "$cask" | grep -qxF "  version \"$VER\"" && printf '%s\n' "$cask" | grep -qxF "  sha256 \"$SHA\""; } \
    || fail "the cask committed in $WORK/tap is not for this zip"
  scan_notes "$WORK/notes.md"
  ASSETS=("$WORK/assets/"*)
  finish_release
  echo "  $WORK can be deleted now"
  exit 0
fi

VER=${1:?usage: scripts/release.sh <version> <notes.md> [ref]}
NOTES=${2:?usage: scripts/release.sh <version> <notes.md> [ref]}
REF=${3:-local-main}

[ -f "$NOTES" ] || fail "no notes file $NOTES"
case "$VER" in [0-9]*.[0-9]*.[0-9]*) ;; *) fail "version must look like 0.1.0" ;; esac
existing=$(release_id "$REPO" "v$VER") || fail "cannot list the releases of $REPO"
[ -z "$existing" ] || fail "a release or draft v$VER already exists (id $existing)"

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
cp "$NOTES" "$WORK/notes.md" || fail "cannot read $NOTES"
/bin/bash "$ROOT/scripts/release-lib-test.sh" > "$WORK/lib-test.log" 2>&1 || { cat "$WORK/lib-test.log" >&2; fail "the release checks fail their own tests"; }
mkdir "$WORK/src"
git -C "$ROOT" archive "$REF" | tar -x -C "$WORK/src" || fail "cannot export $REF"
why=$(cd "$WORK/src" && changelog_ready CHANGELOG.md "$VER") || fail "$why (in $REF)"

cp -R "$WORK/src" "$WORK/scan"
unfolded=$(grep -rIl --exclude=Cargo.lock . "$WORK/scan" | while IFS= read -r f; do
  { fold_dashes < "$f" > "$f.folded" && mv "$f.folded" "$f"; } || echo "$f"
done)
[ -z "$unfolded" ] || fail "could not fold the dashes in: $unfolded"
hits=$(grep -rIEn "$RELEASE_CHECK_DENY" "$WORK/scan" --exclude=Cargo.lock)
case $? in
  0) echo "$hits" | sed "s|$WORK/scan/||" | head -20 >&2; fail "private names in $REF" ;;
  1) ;;
  *) fail "the private-name scan of $REF did not finish" ;;
esac
scan_notes "$NOTES"

echo "release: testing $REF"
(cd "$WORK/src" && cargo test --workspace --locked) > "$WORK/test.log" 2>&1 || { tail -40 "$WORK/test.log" >&2; fail "tests failed"; }

echo "release: building the app"
(cd "$WORK/src" && scripts/make-app.sh "$VER") || fail "packaging failed"
ZIP=$WORK/src/dist/Flowseam-$VER-macos.zip
SHA=$(cat "$ZIP.sha256")
URL=https://github.com/$REPO/releases/download/v$VER/Flowseam-$VER-macos.zip

mkdir "$WORK/assets"
cp "$ZIP" "$WORK/assets/" || fail "cannot copy $ZIP"
if [ "$WIN_EXE" != none ]; then
  cp "$WIN_EXE" "$WORK/assets/Flowseam-$VER-windows-x64.exe" || fail "cannot copy $WIN_EXE"
fi
(cd "$WORK/assets" && shasum -a 256 Flowseam-* > SHA256SUMS) || fail "cannot write SHA256SUMS"
ASSETS=("$WORK/assets/"*)
printf 'VER=%s\nSHA=%s\n' "$VER" "$SHA" > "$WORK/state" || fail "cannot write $WORK/state"

msg() { if [ -n "${RELEASE_TRAILER:-}" ]; then printf '%s\n\n%s\n' "$1" "$RELEASE_TRAILER"; else printf '%s\n' "$1"; fi; }

echo "release: preparing the source and the cask"
gh repo clone "$REPO" "$WORK/pub" -- -q || fail "cannot clone $REPO"
rsync -a --delete --exclude .git --exclude dist "$WORK/src/" "$WORK/pub/" || fail "copy failed"
git -C "$WORK/pub" add -A
git -C "$WORK/pub" commit -q -m "$(msg "Flowseam $VER")" || fail "nothing changed since the last release"
git -C "$WORK/pub" tag "v$VER"
COMMIT=$(git -C "$WORK/pub" rev-parse "v$VER^{commit}") || fail "cannot read the new tag"
printf 'COMMIT=%s\n' "$COMMIT" >> "$WORK/state" || fail "cannot write $WORK/state"

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

  # The settings, this machine's identity and its pairings: after a zap, every computer pairs again.
  zap trash: [
    "~/.config/flowseam",
    "~/Library/Application Support/flowseam",
    "~/Library/Caches/Flowseam",
    "~/Library/Logs/Flowseam",
    "~/Library/Preferences/com.operonlab.flowseam.plist",
  ]

  caveats <<~EOS
    Open Flowseam once and grant Accessibility and Input Monitoring in
    System Settings > Privacy & Security. Updates keep these grants.
    Every machine must run the same Flowseam version.
  EOS
end
EOF
git -C "$WORK/tap" add Casks/flowseam.rb
git -C "$WORK/tap" commit -q -m "$(msg "flowseam $VER")" || fail "cask unchanged"

echo "release: publishing the source"
if ! git -C "$WORK/pub" push -q --atomic origin main "v$VER"; then
  # A push can fail after the server took it; only the remote can say whether this tag is out. --atomic
  # means main moved only if the tag did.
  refs=$(git -C "$WORK/pub" ls-remote origin "refs/tags/v$VER" "refs/tags/v$VER^{}") \
    || stuck "push to $REPO failed, and it cannot be told whether the tag got there"
  tagged=$(printf '%s\n' "$refs" | awk -v t="refs/tags/v$VER" '$2 == t "^{}" { p = $1 } $2 == t { d = $1 } END { print (p != "" ? p : d) }')
  [ -n "$tagged" ] || fail "push to $REPO failed; nothing was published"
  [ "$tagged" = "$COMMIT" ] || fail "push to $REPO failed: it already has a v$VER, on $tagged; nothing of this run was published"
  stuck "push to $REPO reported a failure, but the tag v$VER is there"
fi

finish_release

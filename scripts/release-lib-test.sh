#!/bin/bash
# Tests scripts/release-lib.sh against stand-ins for gh and curl; nothing here talks to GitHub.
#
#   /bin/bash scripts/release-lib-test.sh
#
# Exit 0: every check passed. 1: a check failed. 2: the tests could not run.
set -u
here=$(cd "$(dirname "$0")" && pwd)
. "$here/release-lib.sh"
command -v jq > /dev/null || { echo "release-lib-test: the gh stand-in needs jq; nothing was tested" >&2; exit 2; }

T=$(mktemp -d "${TMPDIR:-/tmp}/release-lib-test.XXXXXX") || exit 2
trap 'rm -rf "$T"' EXIT
mkdir "$T/bin"

# gh: "gh release create" and "gh api", answered from $FAKE, where releases.<page>.json are the pages of the
# release list (only the first without --paginate) and assets.json the files of release 77 or 12. Each call
# is logged. The asset listing answers from assets.<n>.json on its n-th call when that file exists.
# FAKE_CREATE_FAIL, FAKE_CREATE_PARTIAL (only the first file arrives), FAKE_PATCH_NOOP (publishing does
# nothing) and FAKE_GET_FAIL_AFTER_PATCH (GitHub stops answering once asked to publish) stage failures.
cat > "$T/bin/gh" <<'EOF'
#!/bin/bash
if [ "$1 $2" = "release create" ]; then
  tag=$3; shift 3; files=()
  while [ $# -gt 0 ] && [ "${1#--}" = "$1" ]; do files+=("$1"); shift; done
  draft=false flags=
  for a in "$@"; do case $a in --draft) draft=true; flags="$flags --draft" ;; --verify-tag) flags="$flags --verify-tag" ;; esac; done
  echo "release create $tag$flags" >> "$FAKE/gh.log"
  [ -z "${FAKE_CREATE_FAIL:-}" ] || { echo "HTTP 502: Bad Gateway" >&2; exit 1; }
  jq --arg t "$tag" --argjson d "$draft" '. + [{"id": 77, "tag_name": $t, "draft": $d}]' "$FAKE/releases.1.json" > "$FAKE/r.tmp" \
    && mv "$FAKE/r.tmp" "$FAKE/releases.1.json"
  [ -z "${FAKE_CREATE_PARTIAL:-}" ] || files=("${files[0]}")
  sep=; printf '[' > "$FAKE/assets.json"
  for f in "${files[@]}"; do
    printf '%s{"name": "%s", "digest": "sha256:%s"}' "$sep" "$(basename "$f")" "$(shasum -a 256 "$f" | awk '{ print $1 }')" >> "$FAKE/assets.json"
    sep=', '
  done
  echo ']' >> "$FAKE/assets.json"
  [ -z "${FAKE_CREATE_PARTIAL:-}" ] || { echo "upload interrupted" >&2; exit 1; }
  exit 0
fi
[ "$1" = api ] || { echo "fake gh: unsupported: $*" >&2; exit 64; }
shift; method=GET paginate= path= filter=.
while [ $# -gt 0 ]; do
  case $1 in
    -X) method=$2; shift 2 ;;
    --paginate) paginate=1; shift ;;
    --jq) filter=$2; shift 2 ;;
    -F | -f) shift 2 ;;
    -*) echo "fake gh: unsupported flag $1" >&2; exit 64 ;;
    *) path=$1; shift ;;
  esac
done
echo "$method $path" >> "$FAKE/gh.log"
[ -z "${FAKE_GH_FAIL:-}" ] || { echo "HTTP 502: Bad Gateway" >&2; exit 1; }
case "$method $path" in
  "GET repos/"*"/releases?per_page=100")
    for p in "$FAKE"/releases.[0-9]*.json; do jq -r "$filter" "$p"; [ -n "$paginate" ] || break; done ;;
  "GET repos/"*"/releases/"*"/assets?per_page=100")
    n=$(( $(cat "$FAKE/asset-calls" 2> /dev/null || echo 0) + 1 )); echo "$n" > "$FAKE/asset-calls"
    f=$FAKE/assets.$n.json; [ -f "$f" ] || f=$FAKE/assets.json
    jq -r "$filter" "$f" ;;
  "GET repos/"*"/releases/"*)
    [ -z "${FAKE_GET_FAIL_AFTER_PATCH:-}" ] || [ ! -f "$FAKE/patched" ] || { echo "HTTP 502: Bad Gateway" >&2; exit 1; }
    cat "$FAKE"/releases.[0-9]*.json | jq -s -r --argjson id "${path##*/}" "add | map(select(.id == \$id)) | .[0] | $filter" ;;
  "PATCH repos/"*"/releases/"*)
    touch "$FAKE/patched"
    if [ -z "${FAKE_PATCH_NOOP:-}" ]; then
      jq --argjson id "${path##*/}" 'map(if .id == $id then .draft = false else . end)' "$FAKE/releases.1.json" > "$FAKE/r.tmp" \
        && mv "$FAKE/r.tmp" "$FAKE/releases.1.json"
    fi
    echo '{"draft": false}' | jq -r "$filter" ;;
  *) echo "fake gh: unknown call $method $path" >&2; exit 64 ;;
esac
EOF
# curl: fails its first $FAKE_CURL_FAILS calls with a 504, then writes $FAKE_CURL_BODY to the -o file.
cat > "$T/bin/curl" <<'EOF'
#!/bin/bash
out= url=
while [ $# -gt 0 ]; do
  case $1 in -o) out=$2; shift 2 ;; --max-time) shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac
done
n=$(( $(cat "$FAKE/curl-calls" 2> /dev/null || echo 0) + 1 )); echo "$n" > "$FAKE/curl-calls"
echo "$url" >> "$FAKE/curl.log"
if [ "$n" -le "${FAKE_CURL_FAILS:-0}" ]; then echo "curl: (22) The requested URL returned error: 504" >&2; exit 22; fi
cp "$FAKE_CURL_BODY" "$out"
EOF
chmod +x "$T/bin/gh" "$T/bin/curl"
PATH="$T/bin:$PATH"
export RELEASE_DIGEST_PAUSE=0

failures=0
fresh() {
  rm -rf "$T/fake"; mkdir "$T/fake"; export FAKE=$T/fake
  echo '[]' > "$FAKE/releases.1.json"
  unset FAKE_GH_FAIL FAKE_CURL_FAILS FAKE_CURL_BODY FAKE_CREATE_FAIL FAKE_CREATE_PARTIAL FAKE_PATCH_NOOP FAKE_GET_FAIL_AFTER_PATCH
}
# logged <pattern> <want count> <what>: the gh stand-in's log has that many lines matching <pattern>.
logged() {
  local n
  n=$(grep -c -e "$1" "$FAKE/gh.log" 2> /dev/null)
  if [ "${n:-0}" -eq "$2" ]; then echo "ok   $3"; else echo "FAIL $3: ${n:-0} gh calls match '$1', want $2"; failures=$((failures + 1)); fi
}
# expect <status> <what> <command...>: runs the command, keeps its stdout in $out.
expect() {
  local want=$1 what=$2 got
  shift 2
  out=$("$@" 2> "$T/stderr")
  got=$?
  if [ "$got" -eq "$want" ]; then echo "ok   $what"; else echo "FAIL $what: status $got, want $want; said: $out"; failures=$((failures + 1)); fi
}
# said <text> <what>: the last command's stdout contains <text>.
said() {
  case $out in *"$1"*) echo "ok   $2" ;; *) echo "FAIL $2: wanted \"$1\" in: $out"; failures=$((failures + 1)) ;; esac
}
# calls <file> <n> <what>: the stand-in recorded <n> calls.
calls() {
  local n
  n=$(cat "$FAKE/$1" 2> /dev/null || echo 0)
  if [ "$n" -eq "$2" ]; then echo "ok   $3"; else echo "FAIL $3: $n calls, want $2"; failures=$((failures + 1)); fi
}
sha() { shasum -a 256 "$1" | awk '{ print $1 }'; }

echo "changelog_ready"
fresh
c=$T/fake/CHANGELOG.md
printf '# Changelog\n\n## [0.1.4] - 2026-11-09\n\n- A change.\n\n## [0.1.3] - 2026-11-08\n' > "$c"
expect 0 "a dated entry passes" changelog_ready "$c" 0.1.4
expect 1 "a version with no entry fails" changelog_ready "$c" 0.1.5
said "no dated entry" "the failure says the entry is missing"
printf '## [0.1.4]\n\n- A change.\n' > "$c"
expect 1 "an entry without a date fails" changelog_ready "$c" 0.1.4
printf '## [0x1y4] - 2026-11-09\n' > "$c"
expect 1 "the dots in the version are literal" changelog_ready "$c" 0.1.4
printf '## [Unreleased]\n\n## [0.1.4] - 2026-11-09\n\n- A change.\n' > "$c"
expect 0 "an empty [Unreleased] passes" changelog_ready "$c" 0.1.4
printf '## [Unreleased]\n\n- Not moved yet.\n\n## [0.1.4] - 2026-11-09\n\n- A change.\n' > "$c"
expect 1 "changes left under [Unreleased] fail" changelog_ready "$c" 0.1.4
said "[Unreleased]" "the failure names [Unreleased]"
expect 1 "a missing changelog fails" changelog_ready "$T/fake/none.md" 0.1.4

echo "release_id"
fresh
echo '[{"id": 11, "tag_name": "v0.1.3", "draft": false}, {"id": 12, "tag_name": "v0.1.4", "draft": true}]' > "$FAKE/releases.1.json"
expect 0 "a draft is found by its tag" release_id example/app v0.1.4
[ "$out" = 12 ] && echo "ok   it is the draft's id" || { echo "FAIL the id: got '$out', want 12"; failures=$((failures + 1)); }
expect 0 "an unreleased tag is not an error" release_id example/app v0.1.5
[ -z "$out" ] && echo "ok   and gives no id" || { echo "FAIL an unreleased tag gave '$out'"; failures=$((failures + 1)); }
echo '[{"id": 5, "tag_name": "v0.0.9", "draft": false}]' > "$FAKE/releases.2.json"
expect 0 "a release on a later page is found" release_id example/app v0.0.9
[ "$out" = 5 ] && echo "ok   with its id" || { echo "FAIL the id on page 2: got '$out', want 5"; failures=$((failures + 1)); }
export FAKE_GH_FAIL=1
expect 1 "no answer from GitHub fails instead of reading as unreleased" release_id example/app v0.1.5

echo "release_assets_match"
mkdir "$T/files"
printf 'zip bytes' > "$T/files/App-0.1.4.zip"
printf 'exe bytes' > "$T/files/App-0.1.4.exe"
(cd "$T/files" && shasum -a 256 App-0.1.4.zip App-0.1.4.exe > SHA256SUMS)
files=("$T/files/App-0.1.4.zip" "$T/files/App-0.1.4.exe" "$T/files/SHA256SUMS")
zd=sha256:$(sha "$T/files/App-0.1.4.zip") ed=sha256:$(sha "$T/files/App-0.1.4.exe") sd=sha256:$(sha "$T/files/SHA256SUMS")
listing() { # listing <file> <zip digest> <exe digest> <sums digest> [extra asset name]
  local extra=
  [ -z "${5:-}" ] || extra=", {\"name\": \"$5\", \"digest\": \"sha256:00\"}"
  printf '[{"name": "App-0.1.4.zip", "digest": %s}, {"name": "App-0.1.4.exe", "digest": %s}, {"name": "SHA256SUMS", "digest": %s}%s]\n' \
    "$2" "$3" "$4" "$extra" > "$1"
}
fresh
listing "$FAKE/assets.json" "\"$zd\"" "\"$ed\"" "\"$sd\""
expect 0 "matching files pass" release_assets_match example/app 12 "${files[@]}"
logged '^GET repos/example/app/releases/12/assets?per_page=100$' 1 "it asks about that release"
fresh
listing "$FAKE/assets.json" "\"$zd\"" '"sha256:0000"' "\"$sd\""
expect 1 "a changed file fails" release_assets_match example/app 12 "${files[@]}"
said "App-0.1.4.exe: GitHub has sha256:0000" "the failure names that file"
fresh
listing "$FAKE/assets.json" "\"$zd\"" "\"$ed\"" "\"$sd\"" Stray.txt
expect 1 "a file nobody uploaded fails" release_assets_match example/app 12 "${files[@]}"
said "holds 4 files, not the 3 uploaded" "the failure counts the files"
fresh
printf '[{"name": "App-0.1.4.zip", "digest": "%s"}, {"name": "SHA256SUMS", "digest": "%s"}]\n' "$zd" "$sd" > "$FAKE/assets.json"
expect 1 "a file missing from the release fails" release_assets_match example/app 12 "${files[@]}"
said "App-0.1.4.exe: not on release 12" "the failure names the missing file"
fresh
listing "$FAKE/assets.1.json" "\"$zd\"" null "\"$sd\""
listing "$FAKE/assets.json" "\"$zd\"" "\"$ed\"" "\"$sd\""
expect 0 "a digest that arrives late passes" release_assets_match example/app 12 "${files[@]}"
calls asset-calls 2 "after asking once more"
fresh
listing "$FAKE/assets.json" "\"$zd\"" null "\"$sd\""
RELEASE_DIGEST_TRIES=3 expect 1 "a digest that never arrives fails" release_assets_match example/app 12 "${files[@]}"
calls asset-calls 3 "after the given number of tries"
said "gives no digest" "the failure says there is no digest"
fresh
export FAKE_GH_FAIL=1
expect 1 "no answer from GitHub fails" release_assets_match example/app 12 "${files[@]}"

echo "publish_checked"
draft_of() { jq -r --argjson id "$1" 'map(select(.id == $id)) | .[0].draft' "$FAKE/releases.1.json"; }
fresh
expect 0 "with no release, uploads a draft, checks it and publishes it" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
logged '^release create v0.1.4 --draft --verify-tag$' 1 "one draft is made, on the existing tag"
logged '^PATCH repos/example/app/releases/77$' 1 "and published once"
[ "$(draft_of 77)" = false ] && echo "ok   it ends up published" || { echo "FAIL release 77 is still a draft"; failures=$((failures + 1)); }
fresh
export FAKE_CREATE_FAIL=1
expect 1 "a draft that cannot be made fails" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
said "could not be made" "the failure says so"
logged '^PATCH' 0 "and publishes nothing"
fresh
export FAKE_CREATE_PARTIAL=1
expect 1 "an upload cut short fails" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
logged '^PATCH' 0 "and publishes nothing"
unset FAKE_CREATE_PARTIAL
expect 1 "run again, it checks the partial draft instead of making another" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
logged '^release create' 1 "so no second draft is made"
said "draft release 77 does not hold these files" "the failure names the draft"
said "gh api -X DELETE repos/example/app/releases/77" "and how to delete it"
logged '^PATCH' 0 "and it still publishes nothing"
fresh
echo '[{"id": 12, "tag_name": "v0.1.4", "draft": true}]' > "$FAKE/releases.1.json"
listing "$FAKE/assets.json" "\"$zd\"" "\"$ed\"" "\"$sd\""
expect 0 "a matching draft left by an earlier run is published" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
logged '^release create' 0 "without a new upload"
[ "$(draft_of 12)" = false ] && echo "ok   it ends up published" || { echo "FAIL release 12 is still a draft"; failures=$((failures + 1)); }
fresh
echo '[{"id": 12, "tag_name": "v0.1.4", "draft": false}]' > "$FAKE/releases.1.json"
listing "$FAKE/assets.json" "\"$zd\"" "\"$ed\"" "\"$sd\""
expect 0 "a matching release already published passes" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
logged '^release create\|^PATCH' 0 "touching nothing"
fresh
echo '[{"id": 12, "tag_name": "v0.1.4"}]' > "$FAKE/releases.1.json"
listing "$FAKE/assets.json" "\"$zd\"" "\"$ed\"" "\"$sd\""
expect 1 "a release whose draft state GitHub does not give fails" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
said "cannot tell whether release 12 is a draft" "the failure says it cannot tell"
fresh
echo '[{"id": 12, "tag_name": "v0.1.4", "draft": false}]' > "$FAKE/releases.1.json"
listing "$FAKE/assets.json" "\"$zd\"" '"sha256:0000"' "\"$sd\""
expect 1 "a published release with other files fails" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
said "published release 12 does not hold these files" "the failure says it is published"
fresh
export FAKE_PATCH_NOOP=1
expect 1 "a release still a draft after publishing fails" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
said "still a draft" "the failure says so"
fresh
export FAKE_GET_FAIL_AFTER_PATCH=1
expect 1 "no answer after publishing fails as unknown" publish_checked example/app v0.1.4 "App 0.1.4" "$T/files/SHA256SUMS" "${files[@]}"
said "cannot tell whether release 77 was published" "the failure says it cannot tell"

echo "wait_for_download"
url=https://example.invalid/App-0.1.4.zip
want=$(sha "$T/files/App-0.1.4.zip")
fresh
export FAKE_CURL_BODY=$T/files/App-0.1.4.zip
expect 0 "a file that downloads and matches passes" wait_for_download "$url" "$want" 3 0
calls curl-calls 1 "on the first try"
fresh
export FAKE_CURL_BODY=$T/files/App-0.1.4.zip FAKE_CURL_FAILS=2
expect 0 "a URL that answers 504 twice, then serves the file, passes" wait_for_download "$url" "$want" 3 0
calls curl-calls 3 "on the third try"
fresh
export FAKE_CURL_BODY=$T/files/App-0.1.4.zip FAKE_CURL_FAILS=5
expect 1 "a URL that never serves fails with 1" wait_for_download "$url" "$want" 3 0
calls curl-calls 3 "after the given number of tries"
said "did not download in 3 tries" "the failure says it never downloaded"
fresh
export FAKE_CURL_BODY=$T/files/App-0.1.4.exe
expect 2 "a URL serving other bytes fails with 2" wait_for_download "$url" "$want" 3 0
calls curl-calls 1 "without trying again"

if [ "$failures" -ne 0 ]; then
  echo "release-lib-test: $failures failed"
  exit 1
fi
echo "release-lib-test: all passed"

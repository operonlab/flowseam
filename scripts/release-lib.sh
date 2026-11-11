# Checks scripts/release.sh runs between a build and a public release. Each prints why it failed on stdout
# and returns nonzero; scripts/release-lib-test.sh tests them against stand-ins for gh and curl.
# Written for the macOS system bash (3.2).

# changelog_ready <CHANGELOG.md> <version>
# Needs a dated "## [<version>] - YYYY-MM-DD" entry. An "## [Unreleased]" section, if there is one, must be
# empty: whatever it still lists would ship without being listed under the version.
changelog_ready() {
  local file=$1 ver=$2 quoted pending
  [ -f "$file" ] || { echo "no $file"; return 1; }
  quoted=$(printf '%s' "$ver" | sed 's/\./\\./g')
  if ! grep -Eq "^## \[$quoted\] - [0-9]{4}-[0-9]{2}-[0-9]{2}\$" "$file"; then
    echo "$file has no dated entry \"## [$ver] - YYYY-MM-DD\""
    return 1
  fi
  pending=$(awk '/^## \[Unreleased\]/ { u = 1; next } /^## / { u = 0 } u && NF' "$file")
  if [ -n "$pending" ]; then
    echo "$file still lists changes under [Unreleased]; move them into [$ver]"
    return 1
  fi
}

# release_id <owner/repo> <tag>
# The id of the release for <tag>, published or draft, or nothing when there is none. Fails when GitHub
# cannot be asked, so "no answer" never reads as "not released".
release_id() {
  gh api --paginate "repos/$1/releases?per_page=100" --jq ".[] | select(.tag_name == \"$2\") | .id"
}

# publish_checked <owner/repo> <tag> <title> <notes file> <file>...
# Brings <tag> to a published release holding exactly <file>..., and is safe to run again after any failure:
# it uploads a draft only when <tag> has no release, checks whatever release is there against the files, and
# publishes a draft only after that check passes, then reads the result back from GitHub.
publish_checked() {
  local repo=$1 tag=$2 title=$3 notes=$4 id draft why
  shift 4
  id=$(release_id "$repo" "$tag") || { echo "cannot list the releases of $repo"; return 1; }
  if [ -z "$id" ]; then
    echo "release: uploading the files as a draft" >&2
    if ! gh release create "$tag" "$@" --repo "$repo" --title "$title" --notes-file "$notes" --draft --verify-tag > /dev/null; then
      echo "the draft release for $tag could not be made; if GitHub kept part of one, delete it (gh api -X DELETE repos/$repo/releases/<id>)"
      return 1
    fi
    id=$(release_id "$repo" "$tag") || { echo "cannot list the releases of $repo"; return 1; }
  fi
  case $id in
    '' | *[!0-9]*) echo "expected one release for $tag, found '$id'"; return 1 ;;
  esac
  draft=$(gh api "repos/$repo/releases/$id" --jq .draft) || { echo "cannot read release $id"; return 1; }
  case $draft in
    true | false) ;;
    *) echo "cannot tell whether release $id is a draft (GitHub said '$draft')"; return 1 ;;
  esac
  if ! why=$(release_assets_match "$repo" "$id" "$@"); then
    if [ "$draft" = true ]; then
      echo "draft release $id does not hold these files ($why); delete it (gh api -X DELETE repos/$repo/releases/$id)"
    else
      echo "published release $id does not hold these files: $why"
    fi
    return 1
  fi
  [ "$draft" = true ] || return 0
  echo "release: publishing release $id" >&2
  # The answer is read back below rather than from this call, which can fail after GitHub has acted on it.
  gh api -X PATCH "repos/$repo/releases/$id" -F draft=false -f make_latest=true > /dev/null
  draft=$(gh api "repos/$repo/releases/$id" --jq .draft) || { echo "cannot tell whether release $id was published"; return 1; }
  case $draft in
    false) ;;
    true) echo "release $id is still a draft"; return 1 ;;
    *) echo "cannot tell whether release $id was published (GitHub said '$draft')"; return 1 ;;
  esac
}

# release_assets_match <owner/repo> <release id> <file>...
# Succeeds only when the release holds exactly these files and GitHub's sha256 digest of each one equals the
# local file's. GitHub can fill in a digest a moment after the upload, so a missing one is asked again a few
# times, then reported: a missing digest is not a match.
release_assets_match() {
  local repo=$1 id=$2 tries=${RELEASE_DIGEST_TRIES:-5} try listed count f name want got bad=0
  shift 2
  for ((try = 1; ; try++)); do
    listed=$(gh api "repos/$repo/releases/$id/assets?per_page=100" --jq '.[] | "\(.name)\t\(.digest // "")"') \
      || { echo "cannot list the files of release $id"; return 1; }
    if [ "$try" -lt "$tries" ] && printf '%s\n' "$listed" | awk -F '\t' 'NF && $2 == "" { m = 1 } END { exit !m }'; then
      sleep "${RELEASE_DIGEST_PAUSE:-3}"
      continue
    fi
    break
  done
  count=$(printf '%s\n' "$listed" | grep -c .)
  if [ "$count" -ne $# ]; then
    echo "release $id holds $count files, not the $# uploaded"
    bad=1
  fi
  for f in "$@"; do
    name=$(basename "$f")
    want=sha256:$(shasum -a 256 "$f" | awk '{ print $1 }')
    got=$(printf '%s\n' "$listed" | awk -F '\t' -v n="$name" '$1 == n { print $2 }')
    if [ -z "$got" ]; then
      echo "$name: not on release $id, or GitHub gives no digest for it"
      bad=1
    elif [ "$got" != "$want" ]; then
      echo "$name: GitHub has $got, the local file is $want"
      bad=1
    fi
  done
  return $bad
}

# wait_for_download <url> <sha256> <tries> <seconds between tries>
# Right after a release is published, its download URL can answer 504 for minutes, and a Homebrew cask that
# points there fails until it does not. Returns 0 once the URL serves a file with this sha256, 1 if it never
# served one, 2 if it served other bytes (trying again will not fix that).
wait_for_download() {
  local url=$1 want=$2 tries=$3 pause=$4 i tmp got
  tmp=$(mktemp "${TMPDIR:-/tmp}/release-download.XXXXXX") || { echo "cannot make a temporary file"; return 1; }
  for ((i = 1; i <= tries; i++)); do
    if curl -fsSL --max-time 300 -o "$tmp" "$url"; then
      got=$(shasum -a 256 "$tmp" | awk '{ print $1 }')
      rm -f "$tmp"
      [ "$got" = "$want" ] && return 0
      echo "$url serves a file with sha256 $got, not $want"
      return 2
    fi
    [ "$i" -eq 1 ] && echo "release: $url does not download yet; trying every ${pause}s, $tries times in all" >&2
    [ "$i" -lt "$tries" ] && sleep "$pause"
  done
  rm -f "$tmp"
  echo "$url did not download in $tries tries"
  return 1
}

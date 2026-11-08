#!/bin/sh
# Create the GitLab release of v$VERSION from the changelog notes, linking the
# assets published on GitHub. Idempotent: a release that exists is left alone.
#
# Usage: gitlab-release.sh NOTES_FILE DIST_DIR
# Environment: GITLAB_TOKEN, CI_API_V4_URL, CI_PROJECT_ID, VERSION,
#   GITHUB_REPOSITORY (default Pepsit36/herdr-sleep)
set -eu

[ "$#" -eq 2 ] || { echo "usage: gitlab-release.sh NOTES_FILE DIST_DIR" >&2; exit 2; }
notes=$1
dist=$2
sh "$(dirname -- "$0")/require-env.sh" GITLAB_TOKEN CI_API_V4_URL CI_PROJECT_ID VERSION

tag="v$VERSION"
api="$CI_API_V4_URL/projects/$CI_PROJECT_ID/releases"
download="https://github.com/${GITHUB_REPOSITORY:-Pepsit36/herdr-sleep}/releases/download/$tag"

status=$(curl -sS -o /dev/null -w '%{http_code}' --header "PRIVATE-TOKEN: $GITLAB_TOKEN" "$api/$tag")
case $status in
    200) echo "gitlab-release: release $tag already exists"; exit 0 ;;
    404) ;;
    *) echo "gitlab-release: looking for $tag answered HTTP $status" >&2; exit 1 ;;
esac

set -- --data-urlencode "name=$tag" --data-urlencode "tag_name=$tag" --data-urlencode "description@$notes"
for file in "$dist"/herdr-sleep-"$VERSION"-*.tar.gz "$dist"/herdr-sleep-"$VERSION"-*.tar.gz.sha256; do
    [ -f "$file" ] || continue
    name=$(basename -- "$file")
    set -- "$@" --data-urlencode "assets[links][][name]=$name" \
        --data-urlencode "assets[links][][url]=$download/$name"
done

status=$(curl -sS -o /dev/null -w '%{http_code}' --request POST --header "PRIVATE-TOKEN: $GITLAB_TOKEN" "$@" "$api")
if [ "$status" != 201 ]; then
    echo "gitlab-release: creating $tag answered HTTP $status" >&2
    exit 1
fi
echo "gitlab-release: created release $tag"

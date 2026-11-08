#!/bin/sh
# Publish the archives of a canary build as a GitLab generic package. A canary is
# never a tag, a release or a GitHub asset.
#
# Usage: canary-upload.sh DIST_DIR
# Environment: CI_JOB_TOKEN, CI_API_V4_URL, CI_PROJECT_ID, VERSION
set -eu

[ "$#" -eq 1 ] || { echo "usage: canary-upload.sh DIST_DIR" >&2; exit 2; }
dist=$1
sh "$(dirname -- "$0")/require-env.sh" CI_JOB_TOKEN CI_API_V4_URL CI_PROJECT_ID VERSION

base="$CI_API_V4_URL/projects/$CI_PROJECT_ID/packages/generic/herdr-sleep/$VERSION"
count=0
for file in "$dist"/herdr-sleep-"$VERSION"-*.tar.gz "$dist"/herdr-sleep-"$VERSION"-*.tar.gz.sha256; do
    [ -f "$file" ] || continue
    name=$(basename -- "$file")
    status=$(curl -sS -o /dev/null -w '%{http_code}' --header "JOB-TOKEN: $CI_JOB_TOKEN" --upload-file "$file" "$base/$name")
    case $status in
        200 | 201) echo "canary-upload: $name uploaded" ;;
        *) echo "canary-upload: uploading $name answered HTTP $status" >&2; exit 1 ;;
    esac
    count=$((count + 1))
done
if [ "$count" -eq 0 ]; then
    echo "canary-upload: no archive for $VERSION in $dist" >&2
    exit 1
fi
echo "canary-upload: $count files published as herdr-sleep $VERSION"

#!/bin/sh
# Fail, naming the first variable that is unset or empty, before anything changes.
# Usage: require-env.sh VARIABLE...
set -eu

status=0
for name in "$@"; do
    eval "value=\${$name:-}"
    if [ -z "$value" ]; then
        case $name in
            GITLAB_TOKEN)
                hint="Create a project access token (role Maintainer, scopes api and write_repository) and add it as a masked CI/CD variable." ;;
            GITHUB_TOKEN)
                hint="Create a fine-grained token with Contents read and write on the GitHub repository and add it as a masked CI/CD variable." ;;
            *) hint="Set it in the project CI/CD variables." ;;
        esac
        echo "ERROR: $name is not set. $hint" >&2
        status=1
        break
    fi
done
exit "$status"

#!/bin/sh
# Fetches the exact source of every Alpine package in the given package databases into OUT: for each package's
# origin, the aports directory at the commit the package was built from, checked out with git so links and modes
# match, and every remote file its APKBUILD names, checked against the APKBUILD's sha512sums. aports comes from
# Alpine's GitHub mirror, else from Alpine's GitLab; remote files from Alpine's distfiles mirror for BRANCH, else from
# the URL the APKBUILD gives. With OWNER set to uid:gid, OUT ends up owned by it.
# Usage: fetch.sh OUT BRANCH DATABASE...
set -eu

out=$1
branch=$2
shift 2
work=$(mktemp -d)
mkdir -p "$out"
command -v git > /dev/null || apk add --no-cache --quiet git

awk -F: '
    /^o:/ { origin = $2 }
    /^c:/ { commit = $2 }
    /^$/  { if (origin != "" && commit != "") print origin, commit; origin = ""; commit = "" }
    END   { if (origin != "" && commit != "") print origin, commit }
' "$@" | sort -u > "$work/origins"

# Runs COMMAND up to three times; Alpine's servers answer an occasional request with an error.
retry() {
    for attempt in 1 2 3; do
        "$@" && return 0
        sleep $((attempt * 5))
    done
    return 1
}

# Copies directory PATH of aports at COMMIT from REMOTE into DIR.
checkout() {
    repository="$work/aports"
    rm -rf "$repository"
    git init -q "$repository"
    retry git -C "$repository" fetch -q --depth 1 --filter=blob:none "$1" "$3" || return 1
    git -C "$repository" sparse-checkout set --no-cone "/$2/" || return 1
    retry git -C "$repository" checkout -q FETCH_HEAD || return 1
    [ -f "$repository/$2/APKBUILD" ] || return 1
    mkdir -p "$4"
    cp -a "$repository/$2/." "$4/"
}

# Prints the value of VARIABLE after sourcing the APKBUILD in the current directory, as abuild would see it.
# shellcheck disable=SC1091,SC2034 # the APKBUILD is the package's and reads these variables
apkbuild_value() {
    (
        set +eu
        CARCH=x86_64 CBUILD=x86_64-alpine-linux-musl CHOST=x86_64-alpine-linux-musl
        startdir=$PWD srcdir=$PWD/src
        . ./APKBUILD
        eval "printf '%s\n' \"\$$1\""
    )
}

while read -r origin commit; do
    target="$out/$origin"
    fetched=""
    for repository in main community; do
        rm -rf "$target"
        if checkout https://github.com/alpinelinux/aports.git "$repository/$origin" "$commit" "$target" \
            || checkout https://gitlab.alpinelinux.org/alpine/aports.git "$repository/$origin" "$commit" "$target"; then
            fetched=yes
            break
        fi
    done
    [ -n "$fetched" ] || { echo "cannot fetch the aports directory of $origin at $commit" >&2; exit 1; }

    cd "$target"
    for entry in $(apkbuild_value source); do
        case "$entry" in
            *::*://*) file=${entry%%::*} url=${entry#*::} ;;
            *://*) file=${entry##*/} url=$entry ;;
            *) continue ;;
        esac
        wget -q -O "$file" "https://distfiles.alpinelinux.org/distfiles/$branch/$file" \
            || retry wget -q -O "$file" "$url" \
            || { echo "cannot fetch $file for $origin" >&2; exit 1; }
    done
    apkbuild_value sha512sums | grep -v '^[[:space:]]*$' > "$work/sums" || true
    if [ -s "$work/sums" ] && ! sha512sum -c "$work/sums" > /dev/null; then
        echo "a source of $origin does not match its APKBUILD" >&2
        exit 1
    fi
    cd - > /dev/null
    printf '%s  %s\n' "$origin" "$commit" >> "$work/fetched"
done < "$work/origins"

{
    echo "Source of every Alpine package in Contr's WSL guest images."
    echo
    echo "Each directory is the aports directory of one package origin at the commit the package was built from,"
    echo "with every source file its APKBUILD names; abuild builds the package from it. Origin and aports commit:"
    echo
    cat "$work/fetched"
} > "$out/README"

if [ -n "${OWNER:-}" ]; then
    chown -R "$OWNER" "$out"
fi

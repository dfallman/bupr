#!/bin/sh
# Cut a release: bump the patch version (x.y.z → x.y.z+1), commit it, tag
# vX.Y.Z and push. The first release publishes the version already in
# Cargo.toml (0.1.0) without a bump. Pushing the tag runs
# .github/workflows/release.yml, which builds the macOS binaries, publishes
# the GitHub release and updates the formula in dfallman/homebrew-tap.
set -eu

die() {
    echo "release: $*" >&2
    exit 1
}

cd "$(git rev-parse --show-toplevel)"

branch="$(git symbolic-ref --short HEAD)"
[ "$branch" = main ] || die "release from main (you are on $branch)"
[ -z "$(git status --porcelain)" ] || die "the working tree has uncommitted changes"
git fetch --quiet --tags origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
    die "main and origin/main differ; pull or push first"

current="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
echo "$current" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
    die "cannot read x.y.z from Cargo.toml (got \"$current\")"
if [ -n "$(git tag -l 'v[0-9]*')" ]; then
    major="${current%%.*}"
    rest="${current#*.}"
    minor="${rest%%.*}"
    patch="${rest#*.}"
    next="$major.$minor.$((patch + 1))"
else
    next="$current"
fi
tag="v$next"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    die "tag $tag already exists"
fi

echo "Releasing bupr $next ($tag)"
./scripts/check.sh

bumped=no
if [ "$next" != "$current" ]; then
    sed -i '' "s/^version = \"$current\"$/version = \"$next\"/" Cargo.toml
    cargo check --quiet # records the new version in Cargo.lock
    git commit --quiet -m "release $tag" Cargo.toml Cargo.lock
    bumped=yes
fi
git tag -a "$tag" -m "bupr $next"

printf 'Push main and %s to origin? This publishes the release. [y/N] ' "$tag"
read -r answer
case "$answer" in
y | Y | yes) ;;
*)
    undo="git tag -d $tag"
    [ "$bumped" = yes ] && undo="$undo && git reset --hard HEAD~1"
    echo "Not pushed. To undo: $undo"
    exit 1
    ;;
esac
git push --atomic origin main "$tag"
echo "Pushed $tag. Follow the build with: gh run watch -R dfallman/bupr"

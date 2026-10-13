#!/bin/sh
# Builds actuo for macOS arm64 (here) and Linux x86_64 and arm64 (Docker) and
# publishes them as release <commit> of RELEASES_REPO (in
# crates/actuo-core/src/release.rs), with their sha256 files, install.sh (with
# the commit and repo written in) and a `commit` file holding the commit. Actuo
# installs read releases/latest/download/commit once a day to find a newer
# release. <commit> is the repo's last commit, as actuo reports it (ACTUO_COMMIT,
# crates/actuo-core/build.rs).
set -eu
cd "$(dirname "$0")/.."
fail() { echo "release: $*" >&2; exit 1; }
repo=$(sed -n 's/^pub const RELEASES_REPO: &str = "\(.*\)";$/\1/p' crates/actuo-core/src/release.rs)
[ -n "$repo" ] || fail "no RELEASES_REPO line in crates/actuo-core/src/release.rs"

[ -z "$(git status --porcelain)" ] || fail "the repo has uncommitted changes"
commit=$(git log -1 --format=%h --abbrev=7)
git fetch -q origin
git merge-base --is-ancestor HEAD origin/main || fail "HEAD isn't on origin/main; push first"
if gh release view "$commit" -R "$repo" >/dev/null 2>&1; then fail "release $commit exists"; fi
out=target-release/$commit
rm -rf "$out"
mkdir -p "$out"

# Says whether `<binary> --version` names the commit.
check() {
  case "$2" in
    *"($commit)"*) echo "$1: $2" ;;
    *) fail "$1 says \"$2\", not $commit" ;;
  esac
}

echo "== macOS arm64"
cargo build -q --release -p actuo
cp target/release/actuo "$out/actuo-darwin-arm64"
check actuo-darwin-arm64 "$("$out/actuo-darwin-arm64" --version)"

for pair in amd64:x86_64 arm64:arm64; do
  platform=${pair%%:*}
  arch=${pair#*:}
  echo "== Linux $arch (Docker, linux/$platform)"
  docker run --rm --platform "linux/$platform" -v "$PWD":/src -w /src \
    -e CARGO_TARGET_DIR=/tmp/target \
    -v "actuo-linux-$platform-target:/tmp/target" \
    -v "actuo-linux-$platform-cargo:/usr/local/cargo/registry" \
    -v "actuo-linux-$platform-rustup:/usr/local/rustup" \
    rust:1-bookworm bash -c "git config --global --add safe.directory /src \
      && cargo build -q --release -p actuo \
      && cp /tmp/target/release/actuo /src/$out/actuo-linux-$arch \
      && /src/$out/actuo-linux-$arch --version > /src/$out/actuo-linux-$arch.version"
  check "actuo-linux-$arch" "$(cat "$out/actuo-linux-$arch.version")"
  rm "$out/actuo-linux-$arch.version"
done

cd "$out"
for f in actuo-darwin-arm64 actuo-linux-x86_64 actuo-linux-arm64; do
  shasum -a 256 "$f" >"$f.sha256"
done
cd - >/dev/null
sed -e "s/@COMMIT@/$commit/g" -e "s|@REPO@|$repo|g" scripts/install.sh >"$out/install.sh"
echo "$commit" >"$out/commit"

gh release create "$commit" -R "$repo" --title "actuo $commit" \
  --notes "Actuo at commit $commit. Install or update: curl -fsSL https://github.com/$repo/releases/download/$commit/install.sh | sh" \
  "$out"/actuo-darwin-arm64 "$out"/actuo-linux-x86_64 "$out"/actuo-linux-arm64 \
  "$out"/*.sha256 "$out/install.sh" "$out/commit"
echo "published https://github.com/$repo/releases/tag/$commit"

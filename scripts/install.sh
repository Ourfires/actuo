#!/bin/sh
# Installs or updates actuo from release @COMMIT@ of @REPO@.
# scripts/release.sh writes the commit and repo in and publishes this file with the
# release.
#
#   curl -fsSL https://github.com/@REPO@/releases/download/@COMMIT@/install.sh | sh
#
# With `sh -s -- --no-modify-path` it leaves your shell's startup file alone.
#
# For your user (macOS or Linux): checks the download's sha256, then runs
# `actuo service install` through your login shell (so the service gets the PATH
# your shell sets up): it copies actuo to ~/.actuo/bin and (re)starts the
# launch agent or systemd user unit. Not as root: actuo runs as the user that
# installs it. Unless ~/.actuo/bin is on your PATH already, it adds one line that
# puts it there to the startup file of your shell (~/.zshrc, ~/.bashrc on Linux or
# ~/.bash_profile on macOS, fish's conf.d, else ~/.profile) and says so.
set -eu

commit=@COMMIT@
base="https://github.com/@REPO@/releases/download/$commit"
modify_path=1
for arg in "$@"; do
  case "$arg" in
    --no-modify-path) modify_path=0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) asset=actuo-darwin-arm64 ;;
  Linux-x86_64) asset=actuo-linux-x86_64 ;;
  Linux-aarch64 | Linux-arm64) asset=actuo-linux-arm64 ;;
  *) echo "no actuo build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
[ "$(id -u)" != 0 ] || { echo "this installs for your user; not as root" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $asset ($commit)"
curl -fsSL -o "$tmp/actuo" "$base/$asset"
curl -fsSL -o "$tmp/actuo.sha256" "$base/$asset.sha256"
want=$(cut -d' ' -f1 <"$tmp/actuo.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/actuo" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/actuo" | cut -d' ' -f1)
fi
[ "$got" = "$want" ] || { echo "the download's sha256 is $got, the release says $want" >&2; exit 1; }
chmod 755 "$tmp/actuo"

# Actuo's path reaches the shell as $0, never inside the command text.
# shellcheck disable=SC2016
"${SHELL:-/bin/sh}" -l -i -c 'exec "$0" service install' "$tmp/actuo" </dev/null
"${ACTUO_HOME:-$HOME/.actuo}/bin/actuo" --version

# Puts actuo's bin folder on PATH for new shells, once.
add_to_path() {
  bin="${ACTUO_HOME:-$HOME/.actuo}/bin"
  case ":$PATH:" in *":$bin:"*) return 0 ;; esac
  # $HOME stays a variable in the line when the folder is under it.
  case "$bin" in "$HOME"/*) shown="\$HOME/${bin#"$HOME"/}" ;; *) shown="$bin" ;; esac
  line="export PATH=\"$shown:\$PATH\""
  case "$(basename "${SHELL:-sh}")" in
    zsh) rc="${ZDOTDIR:-$HOME}/.zshrc" ;;
    bash)
      if [ "$(uname -s)" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi
      ;;
    fish)
      rc="$HOME/.config/fish/conf.d/actuo.fish"
      line="fish_add_path -g \"$bin\""
      mkdir -p "$(dirname "$rc")"
      ;;
    *) rc="$HOME/.profile" ;;
  esac
  if [ -f "$rc" ] && grep -qF "$line" "$rc"; then return 0; fi
  printf '\n# actuo\n%s\n' "$line" >>"$rc"
  echo "Added $bin to PATH in $rc. Open a new terminal, or run: $line"
}
if [ "$modify_path" = 1 ]; then
  add_to_path
else
  echo "To run actuo by name, put ${ACTUO_HOME:-$HOME/.actuo}/bin on your PATH."
fi
echo "If this release changed the agents' pinned versions, also run: actuo agents install"

#!/usr/bin/env bash
set -euo pipefail

BREW_PREFIX="/home/linuxbrew/.linuxbrew"
SHELLENV_LINE='eval "$('"$BREW_PREFIX"'/bin/brew shellenv)"'

if ! [ -x "$BREW_PREFIX/bin/brew" ]; then
  NONINTERACTIVE=1 /bin/bash -c \
    "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
fi

for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
  [ -f "$rc" ] || continue
  if ! grep -qF "$SHELLENV_LINE" "$rc"; then
    printf '\n# Homebrew\n%s\n' "$SHELLENV_LINE" >> "$rc"
  fi
done

eval "$("$BREW_PREFIX/bin/brew" shellenv)"
brew --version
echo "Homebrew installed at: $BREW_PREFIX"

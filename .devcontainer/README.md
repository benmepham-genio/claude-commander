# Devcontainer

A Debian-based dev container pre-loaded with Homebrew (Linuxbrew) and the
GitHub CLI, for maintaining the `claude-commander` Homebrew tap. Works
identically in GitHub Codespaces and local VS Code via the Dev Containers
extension.

## Opening the container

- **Codespaces:** *Code → Codespaces → Create codespace on this branch*.
  `gh` is pre-authenticated via `GITHUB_TOKEN`.
- **Local VS Code:** install the *Dev Containers* extension, open the repo,
  then run *Dev Containers: Reopen in Container* from the command palette.
  Run `gh auth login` once after the first build.

On first create, `install-brew.sh` runs the official Homebrew installer
non-interactively and wires `brew shellenv` into `~/.bashrc`, `~/.zshrc`,
and `~/.profile`.

## Typical tap workflow

The tap lives in a separate repo (e.g. `sizeak/homebrew-tap`). Clone it
alongside this repo inside the container:

```bash
gh repo clone <user>/homebrew-tap /workspaces/homebrew-tap
cd /workspaces/homebrew-tap
brew audit --strict --online Formula/claude-commander.rb
brew install --build-from-source ./Formula/claude-commander.rb
brew test claude-commander
```

## What's installed

- `brew` at `/home/linuxbrew/.linuxbrew/bin/brew`
- `gh` (GitHub CLI) via the `ghcr.io/devcontainers/features/github-cli` feature
- `git`, `curl`, `build-essential`, `ca-certificates` (from the base image)

`HOMEBREW_NO_ANALYTICS=1` and `HOMEBREW_NO_AUTO_UPDATE=1` are set so the
container stays quiet and reproducible.

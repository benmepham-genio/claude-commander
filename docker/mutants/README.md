# `cargo-mutants` sandbox image

This is **not** a devcontainer. The `.devcontainer/` directory at the repo root is for VS Code dev environments (currently used for brew-tap testing); this image is a one-shot disposable sandbox for running [cargo-mutants](https://mutants.rs/) without putting the host's tmux server, git worktrees, or `~/.config/claude-commander/` at risk.

Use it via `./scripts/mutants.sh` from the repo root. The script builds the image on first invocation, then runs `cargo mutants` with the repo source bind-mounted read-only and results written to `./mutants.out/`.

Pinning: `CARGO_MUTANTS_VERSION` is set as a build-arg in the Dockerfile. Bump it there when you want a newer cargo-mutants.

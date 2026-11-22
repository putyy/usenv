# Usenv Development Guidelines

## Implementation

- Usenv uses Rust and GPUI and supports macOS 13+ on Apple Silicon and Intel. Keep navigation on the left, settings at the bottom left, and actions on the right. Use a cool palette and a transparent title bar. Open a regular window by default and keep the menu bar item available when the window closes. Both interfaces share service actions. Right-align the status dots in the menu, using green for running services and red for stopped services.
- Keep state and actions in `src/controller.rs` and presentation in `src/ui.rs`. Run commands and network queries in the background. Apply sidebar toggles immediately and revert only the affected toggle if saving fails. When checking nested lists, verify that scrolling stays within the list at either end and returns to the page when the pointer leaves.
- Keep Homebrew (`src/brew.rs`) and Mise (`src/runtimes.rs`) usable independently. For Finder launches, locate each executable through `PATH` and common install locations. Use the versions and paths reported by each tool. Pass arguments individually with `Command` and reuse `src/command.rs` for proxy settings and error redaction.
- Let users install a version and select their default as separate actions. Write Homebrew defaults to `defaults.zsh`, let Mise write its global configuration, and resolve Mise-managed Rust toolchain paths with `mise exec`. Keep Composer on the PHP page.
- Use Homebrew to manage the services it installs and the current user's launchd domain for Mise-installed Elasticsearch. Store standalone instances in `~/Library/Application Support/usenv/services/<formula>` and preserve configuration and data when switching modes.

## Workflow and Delivery

- Edit the project once the user has authorized the scope. Ask again only if the scope changes or is unclear. The user verifies changes by default. Run formatting checks, tests, Clippy, or launch the app for UI checks only when explicitly requested.
- Prefer temporary directories and mock commands for verification. Installing or uninstalling real tools, editing the user's shell or Mise configuration, starting or stopping real services, migrating data, and replacing `/Applications/Usenv.app` must be within the authorized scope.
- Write Git commit subjects and bodies in English.
- Use `[package].version` in `Cargo.toml` for both debug and release builds. Run `sh script/bundle.sh --dmg` to produce `dist/Usenv-dev-macos-<arch>.dmg`. Follow the [usenv-release skill](.agents/skills/usenv-release/SKILL.md) when publishing a GitHub release.

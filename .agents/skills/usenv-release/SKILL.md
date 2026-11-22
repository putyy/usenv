---
name: usenv-release
description: Prepare or publish a Usenv GitHub release and track the Apple Silicon and Intel DMGs through upload.
---

# Release Usenv

1. **Identify the target.** Compare the requested repository and commit with `git remote -v`, the update repository in `src/updates.rs`, and the README download link. If the release destination is ambiguous, ask which repository to use. For an explicitly chosen alternate repository, tell the user where the app will still check for updates.
2. **Prepare the release.** Read `[package].version` from the target commit's `Cargo.toml`. Use the matching `vX.Y.Z` tag for a stable release and check the `usenv` entry in `Cargo.lock`. Verify that the commit includes the source, lockfile, packaging resources, and `.github/workflows/macos-dmg.yml`. Write commit subjects and bodies in English. Summarize user-visible changes since the previous release. For a preparation request, return the notes and target commit.
3. **Publish when requested.** Verify that the target SHA exists in the release repository. Check any existing tag and release before publishing. If the tag points elsewhere, report the conflict. Publish an existing draft or proceed to asset checks for an existing published release. To create a new release, use the confirmed repository, tag, full commit SHA, and release notes file.

   ```sh
   gh release create "$tag" --repo "$repo" --target "$sha" --title "$tag" --notes-file "$notes"
   ```

4. **Confirm delivery.** Follow the `Release macOS DMG` workflow run for that tag and SHA. Once both jobs finish, use `gh release view "$tag" --repo "$repo" --json url,assets` to confirm that `Usenv-macos-arm64.dmg` and `Usenv-macos-x86_64.dmg` are uploaded. Return the release URL. Retry a failed job for a transient error. If a fix requires a new commit, report the cause and preserve the published tag.

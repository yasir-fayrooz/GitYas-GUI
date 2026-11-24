# Changelog

Release notes for GitYas, shown in the app's **What's new** dialog after an update and when you click the version
in the bottom-left corner. The file is compiled into the app, so each build shows the notes it shipped with.

How to write it:

- One `## [X.Y.Z] - YYYY-MM-DD` section per release, newest first. The version must match
  `[workspace.package] version` in `Cargo.toml`; the release workflow refuses to build a tag without its section.
- `## [Unreleased]` collects notes for the next version and is never shown in the app. Rename it when you release.
- Group with `### Added`, `### Changed`, `### Fixed`, `### Removed`. Markdown: lists, **bold**, _italic_, `code`,
  [links](https://example.com) and code blocks.
- Write for users, not for the commit log: what they can now do, what behaves differently, what is fixed.

## [Unreleased]

## [1.0.1] - 2026-11-25

### Added

- **Worktrees** in the branches sidebar: see every worktree of the repository and the branch each one is on, add
  one on a new or existing branch (also from a branch's **Check out in new worktree…**), open it in a tab, and
  remove or prune it. Worktrees added or removed from the command line appear on their own.

## [1.0.0] - 2026-11-23

The first open-source release of GitYas: a free, offline-first Git client written in Rust. No account, no sign-in
and nothing sent anywhere except to your own git remotes and the update check.

### Added

- **History**: the commit graph with branches, tags and stashes, commit details, and checkout, branch, tag
  and reset from the graph.
- **Changes**: stage and unstage files or single hunks, commit, stash and pop, discard changes. Drag files out of
  the Changes panel into other apps or folders.
- **Diffs**: hunk, inline and side-by-side views with syntax highlighting and word-level changes. Select diff text
  and copy it with or without the added or removed lines.
- **Merge conflicts**: a built-in merge editor with several layouts.
- **Remotes**: clone, fetch, pull and push over HTTPS (personal access tokens for GitHub, GitLab, Bitbucket and Azure
  DevOps, kept in the system keychain, or your existing git credentials) and SSH (ssh-agent and your default keys).
  Background fetch keeps branches current.
- **Speed mode**: a compact, always-on-top keyboard panel for staging, committing, pushing, branches, stashes and
  tags, with a global shortcut and configurable keys.
- **Profiles**: separate sets of repositories, settings, commit signature and credentials.
- Your commit signature is filled in from your git config (`user.name` / `user.email`) when none is set.
- Light, dark and system themes; zoom.
- Automatic updates from GitHub Releases, verified with a signature before they install.

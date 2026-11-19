# Idea: run plugins as a separate OS user

Status: idea, not scheduled. Noted while designing the node folder and `config/` for manifest v2 (Nov 2026).

## Problem

Local plugins run as the same OS user as the server. So the protection the server puts around a node's files is only against accidents:

- A node's `config/` folder (generated config files and links to uploaded files) is made read-only (`0555` folders, `0444` files on Unix; the read-only attribute on Windows). An accidental write or delete fails, but the owner of a file can always change its permissions back, so a plugin that means to can still change its configuration, or another node's.
- A plugin can read and write everything the server can: the main database (node configs, secrets, tokens), other nodes' folders, spool files and installed packages.

## Idea

Launch plugins as a different, unprivileged OS user (one shared plugin user, or one per plugin or per node), so that permissions actually separate the server from its plugins and plugins from each other:

- The server owns `config/` and the stored uploads; the plugin user can only read them.
- The node folder (apart from `config/`) belongs to the plugin user.
- The database, spool files and other nodes' folders aren't readable by the plugin user.

## What it needs

- The server must be able to start processes as another user:
  - Unix: running as root, or with `CAP_SETUID` / `CAP_SETGID` on Linux, and then `setuid` / `setgid` in the child (`std::os::unix::process::CommandExt::uid` / `gid`).
  - Windows: `CreateProcessAsUser` with a token for a service account or a restricted token. That's more Windows API code, so it would widen the `unsafe` exception (ask first).
- Creating and owning the users (or using a configured existing one); setting owners and ACLs on node folders.
- A config switch (`[supervisor] run_as = "flwrs-plugins"` or similar), off by default, since a plain user install can't do it.
- Supervisor tests that need privileges, so they'd run only in CI containers.

## Related

- The node folder and `config/` design: manifest v2 (`docs/plugin-spec.md`, section 5.3).
- Sandboxing beyond users (containers, seccomp, macOS sandbox profiles) would build on the same launch path.

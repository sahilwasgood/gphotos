# Changelog

## Unreleased

### Added
- `--agent` JSON-lines CLI for AI agents: read commands always available, writes
  gated behind `GPHOTOS_AGENT_WRITE=1` or `--allow-write`. See `AGENT.md`.
- `install.sh`: one-command user install (binary, helpers, desktop entry, icon),
  plus an all-in-one dependency + GitHub clone command for Debian/Ubuntu.
- Transparent logo exports in `assets/`, using the app's own `draw_logo()` art and
  native animation (rotating sun, drifting/bobbing cloud).
- CI workflow: build, Rust unit tests, cookie-helper tests, installer syntax check.
- GitHub issue and pull request templates; repository description and topics.

### Fixed
- Agent CLI now recovers automatically from a stale cached session token instead
  of reporting an empty library.
- Uploads stage for review first; nothing uploads until **Start upload**.
- Failed thumbnail fetches retry after a cooldown; visible-tile texture budget raised.
- Documentation: corrected binary name, test counts, `GPHOTOS_ROOT` default and
  installed command in examples.

### Changed
- README rewritten to lead with installation and first run.
- Logo and icon renamed consistently: the corner mark is the logo, the square
  mark is the icon.
- Repository security features enabled: secret scanning, push protection and
  Dependabot security updates.

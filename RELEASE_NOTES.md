# Release Notes

No PR workflow yet on this repo's first commit — this pushes directly to the
`claude/repo-config-danror` branch to establish the default branch and initial
scaffold. Once there's a real default branch and a second change lands through a
PR, switch to one entry per merged PR (reverse chronological), same convention as
[AISF's RELEASE_NOTES.md](https://github.com/baileyrd/AISF/blob/main/RELEASE_NOTES.md).

---

## Apply repo-config governance scaffold
**2026-08-12**

- **Added:** initial governance file set via the `repo-config` skill — README,
  CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, CHANGELOG, RELEASE_NOTES (this file),
  ARCHITECTURE, an ADR seed, four PR templates, and two issue templates + config.
- **Context:** repo was fully empty (no commits, no manifest, no branches) except
  for a configured `git remote origin` — so `{{OWNER_REPO}}` (`baileyrd/rusty_dbs`)
  and `{{SECURITY_CONTACT}}` (`baileyrd`, the repo owner) resolved for real rather
  than staying placeholders, per the skill's default-to-owner rule. Project intent
  (a Rust reimplementation of
  [baileyrd/Daily-Backup-System](https://github.com/baileyrd/Daily-Backup-System))
  came from the user, since nothing existed yet to infer it from.
- **Known limitation, stated rather than hidden:** the `.github/PULL_REQUEST_TEMPLATE/`,
  `.github/ISSUE_TEMPLATE/`, and CI-workflow assets were missing from this session's
  locally synced copy of the `repo-config` skill — a documented recurring sync gap
  (see the skill's own `RELEASE_NOTES.md`, "Record a sync-gap finding"). Pulling the
  canonical versions from the skill's source repo (`baileyrd/skill_pack`) was blocked
  by this session's repo-access scope, so the PR and issue templates here were
  hand-reconstructed from that same source file's description of their contents
  rather than copied verbatim — worth a diff against `skill_pack` once this session
  has access, to confirm they match. CI workflow was correctly skipped (no manifest
  yet to run against), so that particular gap didn't matter this time.
- No Rust code has landed yet — `ARCHITECTURE.md`'s boundary table and README's
  Getting Started section are left as scaffolding on purpose; there's nothing real
  to put in them until the first slice of the reimplementation exists.

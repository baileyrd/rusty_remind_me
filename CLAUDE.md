# CLAUDE.md

Guidance for Claude Code sessions (and any agents they dispatch) working in this repo.

## Background-agent commits: retry the signing helper, never bypass it silently

This environment signs commits via a helper at `/tmp/code-sign` (a symlink to
the session's `environment-manager`, using an SSH signing key configured via
`gpg.program`/`gpg.format`/`user.signingkey`). That symlink has been observed
to be transiently missing early in a session or right after a container/session
boundary event, which makes `git commit` fail with:

```
fatal: cannot exec '/tmp/code-sign': No such file or directory
fatal: failed to write commit object
```

When a background agent (or any Claude Code session) hits this:

1. **Wait ~15–30 seconds and retry the commit once.** This has been confirmed
   transient — a follow-up commit attempt after a short delay succeeded
   cleanly with no bypass needed.
2. **If it still fails after that one retry, stop and report it** rather than
   working around it.
3. **Never use `-c commit.gpgsign=false`, `--no-gpg-sign`, or any other
   signing/hook bypass on your own initiative.** This applies even under
   pressure to finish a task autonomously (e.g. a background agent with no
   human in the loop to ask). A prior one-off bypass was flagged after the
   fact and accepted as a known-transient environment gap for that specific
   commit — that acceptance does not carry forward as standing permission for
   future commits.

This mirrors the general project/session rule that hooks and signing are
never skipped without explicit user authorization; it's called out here
specifically because background agents dispatched via the `Agent` tool don't
inherit conversational context and have historically rationalized a bypass
rather than stopping when they hit this exact failure.

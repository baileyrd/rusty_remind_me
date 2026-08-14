"""Skool catalog acquisition — the one real Playwright-touching part of
the Rust `dbs-connector-skool` crate (issue #188).

Rust owns every parsing/mapping step (community/course/lesson list
extraction from a `__NEXT_DATA__` blob, course-selector matching, the
raw record -> `BackupItem` mapping) — all of it already ported and
tested as pure functions in `src/lib.rs` before this script existed.
This script's only job is driving a real Chromium page: it navigates
to the pages Rust tells it to, and hands back each page's raw,
undecoded `__NEXT_DATA__` blob for Rust to parse with the exact same
functions its fixture-data tests already exercise.

**Catalog only** (issue #188's scope): no per-lesson page visits, no
resource/video downloads, no `.meta.json` resume tracking, no GitHub
zip archiving. The reference itself has a "catalog only, no downloads"
mode for access-gated communities (`no_download_communities`) — this
script effectively runs every community in that mode. A lesson's
`videoLink`/`videoId`/`resources` are only populated when Skool's
course-tree payload happens to include them; enrichment from each
lesson's own page is a v2, tracked separately.

Two modes, chosen by argv (both share the same login-verified page
loader):

  acquire.py communities <session_dir> <headless> <slugs_json>
    slugs_json: a JSON list of already-slug-normalized community
    slugs, or `[]` to auto-discover every community the logged-in
    account has joined. Navigates each community's classroom page
    (community-level, no course) and returns
    `{"ok": true, "communities": [{"slug": ..., "next_data": ...}, ...]}`
    — one entry per community whose page loaded successfully; a
    community that fails to load is simply omitted (Rust decides what
    that means for reconcile completeness), not a fatal error. Auto-
    discovery finding zero communities is `{"ok": true, "communities": []}`,
    also not an error (mirrors the reference's "no communities to back
    up" warning, not `ConnectorAuthError`).

  acquire.py courses <session_dir> <headless> <pairs_json>
    pairs_json: a JSON list of `[slug, course_slug]` pairs. Navigates
    each course's classroom page and returns
    `{"ok": true, "courses": [{"slug": ..., "course_slug": ..., "next_data": ...}, ...]}`
    — again, a pair whose page fails to load is simply omitted.

Both modes fail the whole call — `{"ok": false, "kind": ..., "message": ...}`
— only for a session-wide problem: Playwright missing, or the session
not logged in (a redirect to `/login` on ANY page, exactly like the
reference's `_require_login`, which aborts the whole `_acquire` rather
than skipping just one page). A single page that loads but has no
`__NEXT_DATA__` (a layout change) is not a session-wide problem, so
it's just omitted from the result, same as a page that errors.
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path
from typing import Any

_BASE = "https://www.skool.com"

# Reads document.getElementById('__NEXT_DATA__') from the current page
# and returns the parsed JSON (or null if the element is absent).
_NEXT_DATA_JS = (
    "() => { const el = document.getElementById('__NEXT_DATA__'); "
    "return el ? JSON.parse(el.textContent) : null; }"
)
# Minimal client-side slug extraction for auto-discovery — just enough
# to know which classroom pages to visit next. The rich, tested parse
# (id/displayName/dedup) is `parse_memberships` in Rust; this only
# needs the bare slug list. `self` here means the page's own
# `window.self` is never touched — this runs inside `page.evaluate`,
# operating on the already-fetched __NEXT_DATA__ object instead.
_DISCOVER_SLUGS_JS = (
    "(nextData) => {"
    " const groups = ((((nextData || {}).props || {}).pageProps || {}).self || {}).allGroups || [];"
    " const slugs = [];"
    " const seen = new Set();"
    " for (const m of groups) {"
    "  const slug = m.name || (m.group && m.group.name);"
    "  if (slug && !seen.has(slug)) { seen.add(slug); slugs.push(slug); }"
    " }"
    " return slugs; }"
)


def _fail(kind: str, message: str) -> None:
    print(json.dumps({"ok": False, "kind": kind, "message": message}))
    sys.exit(1)


def _launch_scrubbed_context(pw: Any, session_dir: Path, *, headless: bool) -> Any:
    """Launch the captured persistent profile, dressed as a regular
    Chrome. Ported from the reference's
    ``_playwright.launch_scrubbed_context`` — see
    `dbs-connector-reddit/scripts/acquire.py`'s identical copy for the
    full rationale (headless Chromium's UA is an instant bot signal)."""
    kwargs: dict[str, Any] = dict(
        user_data_dir=str(session_dir),
        headless=headless,
        args=["--disable-blink-features=AutomationControlled"],
    )
    context = pw.chromium.launch_persistent_context(**kwargs)
    probe = context.pages[0] if context.pages else context.new_page()
    ua = probe.evaluate("() => navigator.userAgent")
    if "HeadlessChrome" in ua:
        context.close()
        kwargs["user_agent"] = ua.replace("HeadlessChrome", "Chrome")
        context = pw.chromium.launch_persistent_context(**kwargs)
    return context


def _load_next_data(page: Any, url: str) -> Any:
    """Navigate and return the page's parsed ``__NEXT_DATA__``, or
    raise on a session-wide login problem. Mirrors the reference's
    ``_load_next_data`` + ``_require_login``: goto (domcontentloaded,
    60s) with up to 3 attempts on a timeout (linear backoff), then a
    login-redirect check that aborts the whole run, not just this page.
    """
    last_exc: Exception | None = None
    for attempt in range(1, 4):
        try:
            page.goto(url, wait_until="domcontentloaded", timeout=60000)
            page.wait_for_selector("#__NEXT_DATA__", state="attached", timeout=30000)
            data = page.evaluate(_NEXT_DATA_JS)
            break
        except Exception as exc:  # noqa: BLE001 - classified below
            last_exc = exc
            is_timeout = "timeout" in type(exc).__name__.lower()
            if not is_timeout or attempt == 3:
                data = None
                break
            time.sleep(attempt * 2)
    else:
        data = None

    if "/login" in (page.url or ""):
        _fail(
            "auth",
            "the captured Skool session is not logged in -- re-run the 'Skool "
            "login' capture (log in, then CLOSE the window).",
        )
    if data is None and last_exc is not None:
        return None  # a page-load failure, not a session problem -- caller skips it
    return data


def _run_communities_mode(page: Any, slugs: list[str]) -> dict[str, Any]:
    if not slugs:
        home_data = _load_next_data(page, f"{_BASE}/")
        slugs = list(page.evaluate(_DISCOVER_SLUGS_JS, home_data)) if home_data else []

    communities = []
    for slug in slugs:
        data = _load_next_data(page, f"{_BASE}/{slug}/classroom")
        if data is None:
            continue
        communities.append({"slug": slug, "next_data": data})
    return {"ok": True, "communities": communities}


def _run_courses_mode(page: Any, pairs: list[list[str]]) -> dict[str, Any]:
    courses = []
    for pair in pairs:
        slug, course_slug = pair[0], pair[1]
        data = _load_next_data(page, f"{_BASE}/{slug}/classroom/{course_slug}")
        if data is None:
            continue
        courses.append({"slug": slug, "course_slug": course_slug, "next_data": data})
    return {"ok": True, "courses": courses}


def main() -> None:
    if len(sys.argv) != 5:
        _fail("config", "usage: acquire.py <communities|courses> <session_dir> <headless> <payload_json>")

    mode = sys.argv[1]
    session_dir = Path(sys.argv[2])
    headless = sys.argv[3].strip().lower() == "true"
    payload = json.loads(sys.argv[4])
    if mode not in ("communities", "courses"):
        _fail("config", f"unknown acquisition mode: {mode!r}")

    try:
        from playwright.sync_api import sync_playwright
    except ImportError as exc:
        _fail(
            "config",
            "the Skool connector needs Playwright; install it with "
            "`pip install playwright` and run `playwright install chromium`. "
            f"({exc})",
        )

    try:
        with sync_playwright() as pw:
            context = _launch_scrubbed_context(pw, session_dir, headless=headless)
            try:
                page = context.new_page()
                if mode == "communities":
                    result = _run_communities_mode(page, payload)
                else:
                    result = _run_courses_mode(page, payload)
            finally:
                context.close()
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001 - last-resort: never crash without JSON
        _fail("transient", f"skool: acquisition failed: {exc}")

    print(json.dumps(result))
    sys.exit(0)


if __name__ == "__main__":
    main()

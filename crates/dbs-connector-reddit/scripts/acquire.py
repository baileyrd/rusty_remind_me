"""Reddit saved-feed acquisition — the one real Playwright-touching part
of the Rust `dbs-connector-reddit` crate (issue #187).

Rust owns everything else (config validation, the raw-listing -> record
mapping, the record -> BackupItem mapping, checkpointing, reconcile) --
this script's only job is to drive a real Chromium page against
reddit.com and hand back the raw JSON `children` the saved-listing API
returns, undecoded, so `record_from_child` in lib.rs (already written
and tested against fixture data) does the actual domain mapping. That
keeps the mapping logic in exactly one place instead of parallel-ported
in Python and Rust.

Invocation: `python3 acquire.py <session_dir> <headless: true|false>
<max_pages> <delay_seconds>`. Always emits exactly one line of JSON to
stdout and nothing else there (diagnostics go to stderr, where Rust
doesn't parse them) so the caller's line-oriented result parsing can't
be confused by, say, Playwright's own startup chatter:

  success: {"ok": true, "account": "<name>", "children": [...]}
  failure: {"ok": false, "kind": "config"|"auth"|"transient"|
            "rate_limited", "message": "..."}

Exit code mirrors `ok`: 0 on success, 1 on failure.
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path
from typing import Any


def _fail(kind: str, message: str) -> None:
    print(json.dumps({"ok": False, "kind": kind, "message": message}))
    sys.exit(1)


def _launch_scrubbed_context(pw: Any, session_dir: Path, *, headless: bool) -> Any:
    """Launch the captured persistent profile, dressed as a regular Chrome.

    Headless Chromium advertises ``HeadlessChrome/<ver>`` in its user
    agent -- an instant bot signal to Reddit's anti-automation edge.
    Probe the launched browser's own UA and, if needed, relaunch once
    with the token scrubbed. Ported from the reference's
    ``_playwright.launch_scrubbed_context``.
    """
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


# Evaluated inside a real reddit.com page: same-origin fetch with the
# page's cookies, returning {status, text}. Text-then-parse (not
# r.json()) so a 200 HTML block page surfaces as a JSON decode error
# instead of a confusing fetch-level exception.
_FETCH_JS = (
    "(u) => fetch(u, {credentials: 'same-origin', headers: {accept: 'application/json'}})"
    ".then(r => r.text().then(t => ({status: r.status, text: t})))"
)


def _page_get(page: Any, url: str) -> tuple[int, str]:
    payload = page.evaluate(_FETCH_JS, url)
    return int(payload["status"]), payload["text"]


def _check_status(status: int, url: str) -> None:
    if 200 <= status < 300:
        return
    if status in (401, 403):
        _fail(
            "auth",
            f"reddit: {url} returned HTTP {status} -- Reddit refused the "
            "authenticated request. Either the session cookies were "
            "rejected (re-run the 'Reddit login' capture) or Reddit's bot "
            "protection is blocking the automated browser; if a fresh "
            "capture still fails, set headless = false for this source "
            "in dbs.toml and retry.",
        )
    if status == 429:
        _fail("rate_limited", f"reddit: rate-limited (HTTP 429) at {url}")
    _fail("transient", f"reddit: HTTP {status} from {url}")


def _verify_login(page: Any) -> str:
    status, text = _page_get(page, "https://www.reddit.com/api/me.json")
    _check_status(status, "https://www.reddit.com/api/me.json")
    try:
        body = json.loads(text) or {}
    except ValueError:
        _fail("transient", "reddit: /api/me.json returned a non-JSON body")
    name = (body.get("data") or {}).get("name")
    if not name:
        _fail(
            "auth",
            "the captured Reddit session is not logged in -- re-run the "
            "'Reddit login' capture. If you sign in with Google, finish "
            "the SSO redirect and make sure reddit.com shows you logged "
            "in BEFORE closing the window, so the session cookie is "
            "persisted.",
        )
    return str(name)


def _walk_saved_json(
    page: Any, name: str, max_pages: int, delay: float
) -> list[dict[str, Any]]:
    seen_ids: set[str] = set()
    children: list[dict[str, Any]] = []
    after: str | None = None

    for _page_num in range(max_pages):
        url = f"https://www.reddit.com/user/{name}/saved.json?limit=100&raw_json=1"
        if after:
            url += f"&after={after}"
        status, text = _page_get(page, url)
        _check_status(status, url)
        try:
            data = (json.loads(text) or {}).get("data") or {}
        except ValueError:
            _fail("transient", f"reddit: {url} returned a non-JSON body")

        for child in data.get("children") or []:
            fullname = (child.get("data") or {}).get("name") or ""
            if fullname and fullname not in seen_ids:  # listings can repeat across pages
                seen_ids.add(fullname)
                children.append(child)

        after = data.get("after")
        if not after or not data.get("children"):
            break
        if delay:
            time.sleep(delay)

    return children


def main() -> None:
    if len(sys.argv) != 5:
        _fail("config", "usage: acquire.py <session_dir> <headless> <max_pages> <delay>")

    session_dir = Path(sys.argv[1])
    headless = sys.argv[2].strip().lower() == "true"
    max_pages = int(sys.argv[3])
    delay = float(sys.argv[4])

    try:
        from playwright.sync_api import sync_playwright
    except ImportError as exc:
        _fail(
            "config",
            "the Reddit connector needs Playwright; install it with "
            "`pip install playwright` and run `playwright install chromium`. "
            f"({exc})",
        )

    try:
        with sync_playwright() as pw:
            context = _launch_scrubbed_context(pw, session_dir, headless=headless)
            try:
                page = context.new_page()
                # Establish a real www.reddit.com document for the
                # same-origin fetches below. Deliberately not checking
                # the navigation status: a blocked page still sets the
                # origin, and the me.json fetch produces the real,
                # actionable error.
                page.goto("https://www.reddit.com/", wait_until="domcontentloaded")
                name = _verify_login(page)
                children = _walk_saved_json(page, name, max_pages, delay)
            finally:
                context.close()
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001 - last-resort: never crash without JSON
        _fail("transient", f"reddit: acquisition failed: {exc}")

    print(json.dumps({"ok": True, "account": name, "children": children}))
    sys.exit(0)


if __name__ == "__main__":
    main()

//! Markdown renderings for tools that previously had only a JSON response (#206).
//!
//! # Why these exist, and why JSON stays the default
//!
//! The reference returns Markdown from these thirteen tools and offers no JSON
//! at all — ten of them have no `response_format` field, and four take no
//! parameters whatsoever. This port returned JSON and offered no Markdown. Both
//! are half a surface.
//!
//! Adding Markdown here rather than replacing JSON is deliberate: JSON was
//! already this port's observable behaviour, so **`response_format` defaults to
//! `json` for these tools and Markdown is opt-in**. Every existing caller keeps
//! working unchanged, and the capability gap against the reference closes.
//!
//! That does mean the *default* output still differs from the reference's. The
//! alternative — flipping the default to Markdown for parity — would break
//! every current caller to imitate a limitation, which is a bad trade.
//!
//! `remind_me_history` is deliberately untouched: it already offers both and
//! already defaults to Markdown, so changing its default is the one place where
//! "JSON by default" would be a regression rather than a no-op.
//!
//! # These are presentation only
//!
//! Nothing here reads the database or changes a value. Each function takes an
//! already-computed response and formats it, so a rendering bug can misreport
//! but cannot corrupt.

use remind_me_core::expansion::{MemorySearchResponse, RelatedMemory};
use remind_me_core::models::{
    CaptureResult, Memory, RevertOutcome, SavedSearch, SetReminderOutcome,
};
use remind_me_core::updater::UpdateStatus;
use remind_me_core::vectors::ReindexResult;
use remind_me_core::wiki::WikiPage;
use remind_me_core::wiki_fs::WikiCompile;

/// Truncate for a one-line summary, on a character boundary.
fn preview(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push('…');
    }
    out
}

pub fn memory_stored(memory: &Memory) -> String {
    format!(
        "✓ Memory stored with id `{}` in category '{}'.",
        memory.id, memory.category
    )
}

pub fn memory_updated(memory: &Memory) -> String {
    format!(
        "✓ Memory `{}` updated.\n\n{}",
        memory.id,
        preview(&memory.content, 200)
    )
}

pub fn revert_outcome(outcome: &RevertOutcome) -> String {
    // Four variants, four different things to tell a caller. "No change" in
    // particular is a success that did nothing, which a generic ✓ would hide.
    match outcome {
        RevertOutcome::Reverted { revision_id } => {
            format!("✓ Reverted to revision {revision_id}.")
        }
        RevertOutcome::NoChange => {
            "No change — the memory already holds that revision's values.".to_string()
        }
        RevertOutcome::MemoryNotFound => "Memory not found.".to_string(),
        RevertOutcome::RevisionNotFound => "Revision not found.".to_string(),
    }
}

pub fn set_reminder_outcome(outcome: &SetReminderOutcome) -> String {
    // Every variant gets its own line rather than a generic "done": clearing a
    // reminder, rejecting an unparseable time and setting one are three
    // different things a caller may need to react to differently.
    match outcome {
        SetReminderOutcome::Set {
            memory_id,
            remind_at,
        } => format!("✓ Reminder set on `{memory_id}` for {remind_at}."),
        SetReminderOutcome::Cleared { memory_id } => {
            format!("✓ Reminder cleared on `{memory_id}`.")
        }
        SetReminderOutcome::NotFound { memory_id } => {
            format!("Memory `{memory_id}` not found.")
        }
        SetReminderOutcome::Rejected { reason } => format!("Reminder rejected: {reason}"),
    }
}

pub fn saved_search(search: &SavedSearch) -> String {
    format!(
        "✓ Saved search '{}' stored for query `{}`.",
        search.name, search.query
    )
}

pub fn saved_search_list(searches: &[SavedSearch]) -> String {
    if searches.is_empty() {
        return "_No saved searches._".to_string();
    }
    let mut out = format!("**{} saved search(es)**\n", searches.len());
    for s in searches {
        out.push_str(&format!("\n- **{}** — `{}`", s.name, s.query));
    }
    out
}

pub fn reindex_result(result: &ReindexResult) -> String {
    let mut out = format!(
        "✓ Reindexed: {} missing, {} embedded, {} chunks created.",
        result.missing, result.embedded, result.chunks_created
    );
    if result.degraded {
        // Surfaced rather than folded into the counts: a degraded run looks
        // like a successful one from the numbers alone.
        out.push_str("\n\n**Degraded** — some embeddings could not be produced.");
    }
    out
}

pub fn update_status(status: &UpdateStatus) -> String {
    if let Some(error) = &status.error {
        return format!("Could not check for updates: {error}");
    }
    if status.update_available {
        format!(
            "**Update available** — {} commit(s) behind.\n\ninstalled {} ({} → {})",
            status.commits_behind,
            status.installed_version,
            status.local_commit,
            status.remote_commit
        )
    } else {
        format!(
            "Up to date at {} ({}).",
            status.installed_version, status.local_commit
        )
    }
}

pub fn capture_result(result: &CaptureResult) -> String {
    let mut out = format!(
        "✓ Captured `{}` — \"{}\" in category '{}'.",
        result.capture_id, result.title, result.category
    );
    if !result.tags.is_empty() {
        out.push_str(&format!("\n\ntags: {}", result.tags.join(", ")));
    }
    out
}

pub fn wiki_compile(outcome: &WikiCompile) -> String {
    // The three variants are three phases, not degrees of success: a brief is
    // work still to do, integrated is work finished, noop is nothing pending.
    match outcome {
        WikiCompile::Brief {
            pending,
            watermark,
            brief,
        } => format!("**{pending} pending** (watermark {watermark})\n\n{brief}"),
        WikiCompile::Integrated {
            sources_marked,
            watermark,
        } => format!("✓ Integrated {sources_marked} source(s); watermark now {watermark}."),
        WikiCompile::Noop { reason, watermark } => {
            format!("Nothing to compile: {reason} (watermark {watermark})")
        }
    }
}

pub fn wiki_page(page: &WikiPage) -> String {
    format!("# {}\n\n{}", page.title, page.content)
}

/// Renders the **enriched** status value, not the bare `ServerStatus`.
///
/// The dispatch layer overwrites `dashboard`, `sync`, `webhook` and `remote`
/// with live state the core crate cannot see on its own (a separate
/// dashboard process's PID file, the sync worker's in-memory counters, the
/// webhook listener, the remote connector's env/token state). Only `mcp` and
/// `embeddings` keep the core crate's tagged `SubsystemStatus` shape
/// (`{"state": "active"}` / `{"state": "not_implemented", ...}`) — the other
/// four are untagged structs (`DashboardStatus`/`SyncWorkerStatus`/
/// `WebhookStatus`/`RemoteStatus`) with no `state` field at all. Reading all
/// six the same generic way rendered `?` for those four regardless of their
/// real state, even while `remind_me_server_status`'s own JSON output was
/// correct — [`subsystem_line`] renders each shape on its own terms instead.
fn subsystem_line(key: &str, v: &serde_json::Value) -> String {
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);

    match key {
        "dashboard" => {
            if b("running") {
                format!("active ({})", s("url").unwrap_or_default())
            } else {
                "not running".to_string()
            }
        }
        "sync" => {
            if !b("enabled") {
                return "disabled".to_string();
            }
            match s("last_error") {
                Some(err) => format!("error: {err} (after {} cycles)", n("cycles")),
                None => format!("active ({} cycles)", n("cycles")),
            }
        }
        "webhook" => {
            if !b("enabled") {
                return "disabled".to_string();
            }
            if !b("running") {
                return match s("start_error") {
                    Some(err) => format!("enabled, not listening: {err}"),
                    None => "enabled, not listening".to_string(),
                };
            }
            format!(
                "active ({}:{}, {} ingested)",
                s("bind").unwrap_or_default(),
                n("port"),
                n("requests_ingested")
            )
        }
        "remote" => {
            if !b("enabled") {
                "disabled".to_string()
            } else {
                format!("active ({}:{})", s("host").unwrap_or_default(), n("port"))
            }
        }
        // `mcp`/`embeddings`: the tagged `SubsystemStatus` shape.
        _ => v
            .get("state")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string(),
    }
}

pub fn server_status(report: &serde_json::Value) -> String {
    let s = |k: &str| -> String {
        report
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let n = |k: &str| -> i64 { report.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };

    let mut out = format!("**rusty-remind-me {}**\n", s("version"));
    out.push_str(&format!(
        "\n- database: {}",
        report
            .get("database_path")
            .and_then(|v| v.as_str())
            .unwrap_or("(in memory, no file)")
    ));
    out.push_str(&format!("\n- memories: {}", n("memory_count")));
    let current = report
        .get("schema_current")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    out.push_str(&format!(
        "\n- schema: v{} (expected v{}){}",
        n("schema_version"),
        n("expected_schema_version"),
        if current { "" } else { " — **MISMATCH**" }
    ));
    out.push_str(&format!("\n- backups: {}", n("backup_count")));
    for key in [
        "mcp",
        "dashboard",
        "embeddings",
        "sync",
        "webhook",
        "remote",
    ] {
        let line = report
            .get(key)
            .map(|v| subsystem_line(key, v))
            .unwrap_or_else(|| "?".to_string());
        out.push_str(&format!("\n- {key}: {line}"));
    }
    let watcher = report.get("watcher");
    let flag = |k: &str| -> bool {
        watcher
            .and_then(|w| w.get(k))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };
    out.push_str(&format!(
        "\n- watcher: {}",
        if flag("running") {
            "running"
        } else if flag("enabled") {
            "configured, not running"
        } else {
            "disabled"
        }
    ));
    out
}

// ---------------------------------------------------------------------------
// Tools that mirror a reference model (#224)
//
// Unlike everything above, these are not additive. The reference already
// returns Markdown from `remind_me_wiki_list` by default and offers JSON as the
// opt-in; this port had it backwards. So these two renderings reproduce the
// reference's layout rather than inventing one -- the text is what a model
// reads, and a different shape is a different prompt.
// ---------------------------------------------------------------------------

/// The wiki index, as `tools/wiki.py:130-134` renders it.
pub fn wiki_page_list(pages: &[WikiPage]) -> String {
    if pages.is_empty() {
        // Verbatim from the reference, pointer to `wiki_compile` included: an
        // empty index is far more often "nothing synthesised yet" than "nothing
        // to synthesise", and the message is what says which.
        return "_The wiki is empty._ Synthesise pages from raw memories with \
                `remind_me_wiki_compile`."
            .to_string();
    }
    let mut lines = vec![format!("## Wiki — {} page(s)", pages.len()), String::new()];
    for p in pages {
        // `[[title]]` wikilinks, and the summary omitted entirely when blank
        // rather than rendered as a trailing em dash with nothing after it.
        let summary = if p.summary.is_empty() {
            String::new()
        } else {
            format!(" — {}", p.summary)
        };
        lines.push(format!("- [[{}]]{}", p.title, summary));
    }
    lines.join("\n")
}

/// The vault vitality report, as `tools/lifecycle.py:69-90` renders it.
pub fn vitality_report(report: &remind_me_core::vitality::VitalityReport) -> String {
    let mut lines = vec![
        "## Vault Vitality Report".to_string(),
        String::new(),
        format!("**Total memories:** {}", report.total_memories),
        format!("**Active:** {}", report.active_count),
        format!("**Dormant:** {}", report.dormant_count),
        format!("**Vault health:** {}", report.vault_health_score),
        // `:.2f` in the reference. Two places, not Rust's default float
        // formatting, which would print `0.8333333333333334`.
        format!("**Average vitality:** {:.2}", report.average_vitality),
        String::new(),
        "### Vitality Distribution".to_string(),
        String::new(),
    ];
    for (label, count) in &report.vitality_buckets {
        // The bar caps at 40 so one enormous bucket cannot produce a line
        // thousands of characters wide -- the reference's `min(count, 40)`.
        let bar = "#".repeat((*count).min(40));
        lines.push(format!("  {label}: {bar} ({count})"));
    }
    lines.push(String::new());
    lines.push("### Memory Type Distribution".to_string());
    lines.push(String::new());
    // `decay_distribution` is a BTreeMap, so iteration is already sorted --
    // matching the reference's explicit `sorted(...)`.
    for (kind, count) in &report.decay_distribution {
        lines.push(format!("- **{kind}**: {count}"));
    }
    lines.join("\n")
}

/// A search response, as close to `tools/search.py:936-992` as this port's data
/// allows.
///
/// # What is faithful, and what is missing
///
/// Reproduced exactly: the results themselves (`_fmt_memory_md`, via
/// [`remind_me_core::reminders::render_memories_markdown`]), the budget line in
/// both its trimmed and untrimmed forms, the `_No memories found._` empty case,
/// the `\n---\n` joiner, and the three expansion sections.
///
/// **Deliberately absent, because this port does not track the inputs:**
///
/// - the per-hit method badge (`⚡ hybrid` / `🔮 semantic` / `🔤 keyword`) and
///   `distance: N`, which need a per-result search-method tag,
/// - the `_Tiers: N keyword, N semantic, N hybrid | N dormant excluded_`
///   footer, which needs a tier breakdown and a dormant-exclusion count,
/// - the `verbose` debug-signal line, which needs per-result rank positions.
///
/// None of those exist anywhere in `remind_me_core`. Adding them is search
/// pipeline work rather than rendering work, so this deliberately renders less
/// than the reference rather than inventing substitutes that would look right
/// and mean something else.
pub fn search_response(res: &MemorySearchResponse) -> String {
    if res.memories.is_empty() {
        return "_No memories found._".to_string();
    }

    let mut parts: Vec<String> = Vec::new();
    // The reference names the retrieval method here. Without a per-result
    // method tag this port cannot say which ran, and guessing "hybrid" would be
    // a claim rather than a report -- so the count is stated and the method is
    // not.
    parts.push(format!("**{} results**", res.returned));
    if res.trimmed > 0 {
        parts.push(format!(
            "_{} of {} candidates (trimmed {}, ~{}/{} tokens)_\n",
            res.returned, res.total_candidates, res.trimmed, res.tokens_used, res.budget
        ));
    } else {
        parts.push(format!(
            "_~{} tokens used (budget: {})_\n",
            res.tokens_used, res.budget
        ));
    }

    let memories: Vec<_> = res.memories.iter().map(|r| r.memory.clone()).collect();
    parts.push(remind_me_core::reminders::render_memories_markdown(
        &memories,
    ));

    if let Some(related) = res.related_via_entities.as_ref().filter(|r| !r.is_empty()) {
        parts.push(expansion_section(
            &format!(
                "**Related via entities** (1-hop expansion, max {}):",
                remind_me_core::expansion::EXPANSION_CAP
            ),
            related,
            true,
        ));
    }
    if let Some(related) = res.related_via_neighbors.as_ref().filter(|r| !r.is_empty()) {
        parts.push(expansion_section(
            "**Related via document neighbors**:",
            related,
            false,
        ));
    }
    if let Some(related) = res
        .related_via_co_retrieval
        .as_ref()
        .filter(|r| !r.is_empty())
    {
        parts.push(expansion_section(
            "**Related via co-retrieval**:",
            related,
            false,
        ));
    }

    parts.join("\n---\n")
}

/// One expansion block, matching `_fmt_expansion_md`'s per-item shape.
fn expansion_section(heading: &str, related: &[RelatedMemory], via_entities: bool) -> String {
    let mut lines = vec![heading.to_string()];
    for item in related {
        // `" ".join(str(...).split())` in the reference: collapse all runs of
        // whitespace, including newlines, so a multi-line memory does not break
        // the list item across lines.
        let snippet: String = item
            .content_snippet
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // Truncated at 120 *characters*, not bytes -- slicing a String by byte
        // index would panic mid-codepoint on any non-ASCII memory.
        let snippet = if snippet.chars().count() > 120 {
            format!("{}…", snippet.chars().take(120).collect::<String>())
        } else {
            snippet
        };
        if via_entities && !item.via_entities.is_empty() {
            lines.push(format!(
                "- `{}` {} _(via: {})_",
                item.id,
                snippet,
                item.via_entities.join(", ")
            ));
        } else {
            lines.push(format!("- `{}` {}", item.id, snippet));
        }
    }
    lines.join("\n")
}

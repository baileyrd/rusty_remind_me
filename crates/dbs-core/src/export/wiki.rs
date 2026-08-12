//! Wiki exporter — markdown shaped for a remind_me-style wiki, zipped.
//!
//! Mirrors `src/dbs/export/wiki.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). The wiki is a *synthesis* layer ("your
//! synthesised, cross-linked knowledge base distilled from raw
//! memories — not a copy of them"), which is why this is a separate
//! format from `obsidian`: that one mirrors items one-note-per-item for
//! a folder watcher, this one emits pages a wiki can actually adopt — a
//! stable slug, a title that doubles as identity, an opening summary
//! sentence, and `[[wikilinks]]` between pages.
//!
//! Layout inside the zip:
//! ```text
//! pages/<slug>.md   # one page per item, or per source/tag hub
//! index.md          # generated table of contents, all pages wikilinked
//! manifest.json     # same shape as the archive exporter's (#58)
//! ```
//!
//! Grouping ([`ExportQuery::wiki_grouping`](crate::storage::ExportQuery)):
//! `"item"` renders one page per item (tags/source as plain metadata,
//! no hub pages to link to); `"topic"` (the default) renders one page
//! per source and one per grouping value, each listing its items inline
//! and cross-linked to the other. Per-source
//! [`ExportProfile`](crate::export_profile::ExportProfile)s (reachable
//! via [`ExportSource::profiles`]) can override the export-wide
//! grouping per source (`page_per`) and name real grouping axes
//! (`group_by`) instead of collapsing onto the generic `Tag:` namespace.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Write};

use serde_json::{json, Map, Value};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::errors::DbsError;
use crate::export_profile::{axis_label, group_values, raw_value, ExportProfile};
use crate::storage::{ExportQuery, ItemRow};
use crate::timeutil::iso_z;

use super::{ExportResult, ExportSource, Exporter};

const GROUPINGS: &[&str] = &["item", "topic"];

/// Body text pulled onto a hub page as a one-line excerpt.
const EXCERPT_CHARS: usize = 200;

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn truthy_field<'a>(row: &'a ItemRow, key: &str) -> Option<&'a Value> {
    row.get(key).filter(|v| is_truthy(v))
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Lowercase kebab slug — the page identity both wikis key on.
fn slugify(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::new();
    let mut in_run = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "page".to_string()
    } else {
        trimmed.to_string()
    }
}

fn yaml_scalar(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null) => String::new(),
        Some(v) => display_value(v),
    };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let escaped = flat.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn yaml_scalar_str(text: &str) -> String {
    yaml_scalar(Some(&Value::String(text.to_string())))
}

fn yaml_list_strs(values: &[String]) -> String {
    if values.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = values.iter().map(|v| yaml_scalar_str(v)).collect();
    format!("[{}]", items.join(", "))
}

fn excerpt(body: Option<&Value>) -> String {
    let Some(body) = body.filter(|v| is_truthy(v)) else {
        return String::new();
    };
    let text = display_value(body);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > EXCERPT_CHARS {
        let truncated: String = flat.chars().take(EXCERPT_CHARS - 1).collect();
        format!("{truncated}…")
    } else {
        flat
    }
}

/// Flattens to one line and softens link brackets, as `MarkdownExporter` does.
fn md_inline(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.replace('[', "\\[").replace(']', "\\]")
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[derive(Clone)]
struct Record {
    title: String,
    url: Option<String>,
    excerpt: String,
    source: String,
    deleted: bool,
}

enum FrontValue {
    Str(String),
    List(Vec<String>),
    Bool(bool),
    Int(i64),
    Raw(Option<Value>),
}

fn render_front_value(key: &str, value: &FrontValue) -> String {
    match value {
        FrontValue::List(items) => format!("{key}: {}", yaml_list_strs(items)),
        FrontValue::Bool(b) => format!("{key}: {}", if *b { "true" } else { "false" }),
        FrontValue::Int(n) => format!("{key}: {n}"),
        FrontValue::Str(s) => format!("{key}: {}", yaml_scalar_str(s)),
        FrontValue::Raw(v) => format!("{key}: {}", yaml_scalar(v.as_ref())),
    }
}

/// One rendered wiki page, pre-slug-collision-resolution.
struct Page {
    slug: String,
    title: String,
    topic: String,
    front: Vec<(&'static str, FrontValue)>,
    body: Vec<String>,
}

impl Page {
    fn render(&self) -> String {
        let mut lines = vec!["---".to_string()];
        lines.push(format!("slug: {}", yaml_scalar_str(&self.slug)));
        lines.push(format!("title: {}", yaml_scalar_str(&self.title)));
        lines.push(format!("topic: {}", yaml_scalar_str(&self.topic)));
        for (key, value) in &self.front {
            lines.push(render_front_value(key, value));
        }
        lines.push("---".to_string());
        lines.push(String::new());
        // The H1 is emitted explicitly rather than left to the consumer:
        // the Python wiki only *adds* one when absent, and this one never does.
        lines.push(format!("# {}", md_inline(&self.title)));
        lines.push(String::new());
        lines.extend(self.body.iter().cloned());
        let joined = lines.join("\n");
        format!("{}\n", joined.trim_end())
    }
}

pub struct WikiExporter;

impl Exporter for WikiExporter {
    fn format(&self) -> &'static str {
        "wiki"
    }

    fn media_type(&self) -> &'static str {
        "application/zip"
    }

    fn file_ext(&self) -> &'static str {
        ".zip"
    }

    fn write(
        &self,
        source: &dyn ExportSource,
        out: &mut dyn Write,
        query: &ExportQuery,
    ) -> Result<ExportResult, DbsError> {
        let raw_grouping = if query.wiki_grouping.is_empty() {
            "topic"
        } else {
            query.wiki_grouping.as_str()
        };
        let grouping = raw_grouping.to_lowercase();
        if !GROUPINGS.contains(&grouping.as_str()) {
            return Err(DbsError::Config(format!(
                "unknown wiki_grouping {:?}. Available: {GROUPINGS:?}",
                query.wiki_grouping
            )));
        }

        let profiles = source.profiles();
        let mut taken: HashSet<String> = HashSet::new();
        let (pages, item_count, by_source) = build_pages(source, &grouping, &profiles, &mut taken);

        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut zf = ZipWriter::new(Cursor::new(Vec::new()));

        for page in &pages {
            zf.start_file(format!("pages/{}.md", page.slug), options)
                .map_err(zip_err)?;
            zf.write_all(page.render().as_bytes()).map_err(io_err)?;
        }

        zf.start_file("index.md", options).map_err(zip_err)?;
        zf.write_all(render_index(&pages, &grouping).as_bytes())
            .map_err(io_err)?;

        let manifest = build_manifest(
            source.manifest(),
            query,
            &grouping,
            item_count,
            pages.len() as u64,
            &by_source,
        );
        let manifest_text = serde_json::to_string_pretty(&manifest)
            .map_err(|e| DbsError::Storage(format!("failed to encode export manifest: {e}")))?;
        zf.start_file("manifest.json", options).map_err(zip_err)?;
        zf.write_all(manifest_text.as_bytes()).map_err(io_err)?;

        let cursor = zf.finish().map_err(zip_err)?;
        out.write_all(&cursor.into_inner()).map_err(io_err)?;

        let extra = HashMap::from([
            (
                "by_source".to_string(),
                serde_json::to_value(&by_source).unwrap_or(Value::Null),
            ),
            ("pages".to_string(), Value::from(pages.len() as u64)),
            ("grouping".to_string(), Value::from(grouping)),
        ]);

        Ok(ExportResult {
            format: self.format().to_string(),
            item_count,
            extra,
            ..Default::default()
        })
    }
}

/// One streaming pass, routing each row by its source's profile.
///
/// Granularity is per-source (`page_per` overrides the export-wide
/// grouping), so a single export can give every item its own page for
/// one source while collapsing another onto tag hubs. That rules out
/// separate item-mode and topic-mode passes — both shapes can be live
/// at once, so rows are routed as they arrive.
fn build_pages(
    source: &dyn ExportSource,
    grouping: &str,
    profiles: &HashMap<String, ExportProfile>,
    taken: &mut HashSet<String>,
) -> (Vec<Page>, u64, HashMap<String, u64>) {
    let mut by_source: HashMap<String, u64> = HashMap::new();
    let mut count: u64 = 0;

    let mut item_rows: Vec<(ItemRow, ExportProfile, String)> = Vec::new();
    let mut hubs: HashMap<String, HashMap<(String, String), Vec<Record>>> = HashMap::new();
    let mut untagged: HashMap<String, Vec<Record>> = HashMap::new();
    let mut axes: HashMap<(String, String), Vec<Record>> = HashMap::new();

    for row in source.items() {
        let src = truthy_field(&row, "source")
            .map(display_value)
            .unwrap_or_else(|| "unknown".to_string());
        let profile = profiles.get(&src).cloned().unwrap_or_default();
        *by_source.entry(src.clone()).or_insert(0) += 1;
        count += 1;

        let effective_grouping = profile
            .page_per
            .clone()
            .unwrap_or_else(|| grouping.to_string());
        if effective_grouping == "item" {
            item_rows.push((row, profile, src));
            continue;
        }

        let title = truthy_field(&row, "title")
            .or_else(|| truthy_field(&row, "url"))
            .or_else(|| truthy_field(&row, "external_id"))
            .map(display_value)
            .unwrap_or_else(|| "item".to_string());
        let url = truthy_field(&row, "url").map(display_value);
        let record = Record {
            title,
            url,
            excerpt: excerpt(body_text(&row, &profile).as_ref()),
            source: src.clone(),
            deleted: truthy_field(&row, "deleted").is_some(),
        };

        let buckets = hubs.entry(src.clone()).or_default();
        let mut placed = false;
        for key in axis_values(&row, &profile) {
            buckets.entry(key.clone()).or_default().push(record.clone());
            axes.entry(key).or_default().push(record.clone());
            placed = true;
        }
        if !placed {
            untagged.entry(src).or_default().push(record);
        }
    }

    let mut pages: Vec<Page> = Vec::new();

    let mut srcs: BTreeSet<String> = hubs.keys().cloned().collect();
    srcs.extend(untagged.keys().cloned());
    for src in srcs {
        let empty_buckets: HashMap<(String, String), Vec<Record>> = HashMap::new();
        let buckets = hubs.get(&src).unwrap_or(&empty_buckets);
        let mut keys: Vec<(String, String)> = buckets.keys().cloned().collect();
        keys.sort();
        let slug = unique_slug(&format!("source-{}", slugify(&src)), None, taken);
        let axis_labels: Vec<String> = keys
            .iter()
            .map(|(l, _)| l.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let total = *by_source.get(&src).unwrap_or(&0);
        let front = vec![
            ("dbs_source", FrontValue::Str(src.clone())),
            ("dbs_item_count", FrontValue::Int(total as i64)),
            ("dbs_axes", FrontValue::List(axis_labels)),
        ];
        let body = source_body(&src, total, buckets, &keys, untagged.get(&src));
        pages.push(Page {
            slug,
            title: format!("Source: {src}"),
            topic: "source".to_string(),
            front,
            body,
        });
    }

    let mut axis_keys: Vec<(String, String)> = axes.keys().cloned().collect();
    axis_keys.sort();
    for (label, value) in axis_keys {
        let records = &axes[&(label.clone(), value.clone())];
        let slug = unique_slug(
            &format!("{}-{}", slugify(&label), slugify(&value)),
            None,
            taken,
        );
        let srcs: Vec<String> = records
            .iter()
            .map(|r| r.source.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let front = vec![
            ("dbs_item_count", FrontValue::Int(records.len() as i64)),
            ("dbs_sources", FrontValue::List(srcs.clone())),
        ];
        let body = axis_body(&label, &value, records, &srcs);
        pages.push(Page {
            slug,
            title: format!("{label}: {value}"),
            topic: label.to_lowercase(),
            front,
            body,
        });
    }

    for (row, profile, src) in item_rows {
        pages.push(item_page(&row, &profile, &src, taken));
    }

    (pages, count, by_source)
}

/// The item's prose, per the profile's declared fields. Falls back to
/// the normalized `body` column — which is also what happens under
/// `--no-raw`, where the named fields simply aren't in the row.
fn body_text(row: &ItemRow, profile: &ExportProfile) -> Option<Value> {
    for path in &profile.body_from {
        if let Some(v) = raw_value(row, path) {
            if is_truthy(&v) {
                return Some(v);
            }
        }
    }
    row.get("body").cloned()
}

/// `(axis label, value)` pairs this row belongs on. With no declared
/// axes — or under `--no-raw`, where none resolve — falls back to the
/// row's own `tags` so grouping still works.
fn axis_values(row: &ItemRow, profile: &ExportProfile) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for path in &profile.group_by {
        let label = axis_label(path);
        for value in group_values(row, path) {
            out.push((label.clone(), value));
        }
    }
    if out.is_empty() {
        if let Some(tags) = row.get("tags").and_then(|v| v.as_array()) {
            for t in tags {
                let s = display_value(t);
                if !s.trim().is_empty() {
                    out.push(("Tag".to_string(), s));
                }
            }
        }
    }
    out
}

/// Stable slug, disambiguated the way the obsidian exporter
/// disambiguates filenames: fall back to `extra`, then a counter.
fn unique_slug(base: &str, extra: Option<&str>, taken: &mut HashSet<String>) -> String {
    if taken.insert(base.to_string()) {
        return base.to_string();
    }
    if let Some(extra) = extra.filter(|e| !e.is_empty()) {
        let candidate = format!("{base}-{}", slugify(extra));
        if taken.insert(candidate.clone()) {
            return candidate;
        }
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

fn item_page(
    row: &ItemRow,
    profile: &ExportProfile,
    src: &str,
    taken: &mut HashSet<String>,
) -> Page {
    let title = truthy_field(row, "title")
        .or_else(|| truthy_field(row, "url"))
        .or_else(|| truthy_field(row, "external_id"))
        .map(display_value)
        .unwrap_or_else(|| "item".to_string());
    let base: String = slugify(&title).chars().take(80).collect();
    let extra_id = truthy_field(row, "external_id").map(display_value);
    let slug = unique_slug(&base, extra_id.as_deref(), taken);

    let tags: Vec<String> = row
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(display_value).collect())
        .unwrap_or_default();

    let mut front: Vec<(&'static str, FrontValue)> = vec![
        ("tags", FrontValue::List(tags.clone())),
        ("dbs_source", FrontValue::Str(src.to_string())),
        (
            "dbs_external_id",
            FrontValue::Raw(row.get("external_id").cloned()),
        ),
        (
            "dbs_item_kind",
            FrontValue::Raw(row.get("item_kind").cloned()),
        ),
        ("dbs_url", FrontValue::Raw(row.get("url").cloned())),
        (
            "dbs_created_at",
            FrontValue::Raw(row.get("created_at").cloned()),
        ),
    ];
    if truthy_field(row, "deleted").is_some() {
        front.push(("dbs_deleted", FrontValue::Bool(true)));
    }

    let body_text_value = body_text(row, profile);
    let body = item_body(row, &tags, src, body_text_value.as_ref());
    Page {
        slug,
        title,
        topic: src.to_string(),
        front,
        body,
    }
}

fn item_body(row: &ItemRow, tags: &[String], src: &str, body_text: Option<&Value>) -> Vec<String> {
    let kind = truthy_field(row, "item_kind")
        .map(display_value)
        .unwrap_or_else(|| "item".to_string());
    let created_raw = row.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
    let created: String = created_raw.chars().take(10).collect();
    let mut summary = format!("A `{kind}` backed up from the `{src}` source");
    if created.is_empty() {
        summary.push('.');
    } else {
        summary.push_str(&format!(", created {created}."));
    }

    let mut lines = vec![summary, String::new()];
    if truthy_field(row, "deleted").is_some() {
        lines.push("> This item is marked deleted upstream.".to_string());
        lines.push(String::new());
    }
    if let Some(bt) = body_text.filter(|v| is_truthy(v)) {
        lines.push(display_value(bt).trim().to_string());
        lines.push(String::new());
    }
    if let Some(url) = truthy_field(row, "url") {
        lines.push(format!("Source: <{}>", display_value(url)));
        lines.push(String::new());
    }
    if !tags.is_empty() {
        let rendered: Vec<String> = tags.iter().map(|t| format!("`{t}`")).collect();
        lines.push(format!("Tags: {}", rendered.join(", ")));
        lines.push(String::new());
    }
    lines
}

fn bullet(record: &Record, suffix: &str) -> String {
    let title = md_inline(&record.title);
    let mut line = match &record.url {
        Some(u) => format!("- [{title}]({u})"),
        None => format!("- {title}"),
    };
    if !suffix.is_empty() {
        line.push_str(&format!(" — {suffix}"));
    }
    if !record.excerpt.is_empty() {
        line.push_str(&format!(" — {}", md_inline(&record.excerpt)));
    }
    if record.deleted {
        line.push_str(" _(deleted upstream)_");
    }
    line
}

fn source_body(
    src: &str,
    total: u64,
    buckets: &HashMap<(String, String), Vec<Record>>,
    keys: &[(String, String)],
    untagged: Option<&Vec<Record>>,
) -> Vec<String> {
    let labels: BTreeSet<String> = keys.iter().map(|(l, _)| l.clone()).collect();
    let axis_desc = if labels.is_empty() {
        "no axis".to_string()
    } else {
        labels
            .iter()
            .map(|l| l.to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec![
        format!(
            "{} backed up from the `{src}` source, grouped by {axis_desc}.",
            plural(total, "item")
        ),
        String::new(),
    ];
    for (label, value) in keys {
        lines.push(format!("## {}", md_inline(&format!("{label}: {value}"))));
        lines.push(String::new());
        if let Some(records) = buckets.get(&(label.clone(), value.clone())) {
            for record in records {
                lines.push(bullet(record, ""));
            }
        }
        lines.push(String::new());
    }
    if let Some(untagged_records) = untagged.filter(|r| !r.is_empty()) {
        lines.push("## Ungrouped".to_string());
        lines.push(String::new());
        for record in untagged_records {
            lines.push(bullet(record, ""));
        }
        lines.push(String::new());
    }
    if !keys.is_empty() {
        let related: Vec<String> = keys.iter().map(|(l, v)| format!("[[{l}: {v}]]")).collect();
        lines.push(format!("Related: {}", related.join(" · ")));
        lines.push(String::new());
    }
    lines
}

fn axis_body(label: &str, value: &str, records: &[Record], srcs: &[String]) -> Vec<String> {
    let mut lines = vec![
        format!(
            "{} with {} `{value}`, from {}.",
            plural(records.len() as u64, "item"),
            label.to_lowercase(),
            plural(srcs.len() as u64, "source")
        ),
        String::new(),
    ];
    for record in records {
        let suffix = format!("from [[Source: {}]]", record.source);
        lines.push(bullet(record, &suffix));
    }
    lines.push(String::new());
    let sources_line = srcs
        .iter()
        .map(|s| format!("[[Source: {s}]]"))
        .collect::<Vec<_>>()
        .join(" · ");
    lines.push(format!("Sources: {sources_line}"));
    lines.push(String::new());
    lines
}

fn render_index(pages: &[Page], grouping: &str) -> String {
    let mut lines = vec!["---".to_string()];
    lines.push("slug: \"index\"".to_string());
    lines.push("title: \"Index\"".to_string());
    lines.push("topic: \"index\"".to_string());
    lines.push(format!("dbs_page_count: {}", pages.len()));
    lines.push(format!("dbs_wiki_grouping: {}", yaml_scalar_str(grouping)));
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push("# Index".to_string());
    lines.push(String::new());
    lines.push(format!(
        "{} exported from the daily-backup-system database, grouped by `{grouping}`.",
        plural(pages.len() as u64, "page")
    ));
    lines.push(String::new());

    let mut by_topic: BTreeMap<String, Vec<&Page>> = BTreeMap::new();
    for page in pages {
        by_topic.entry(page.topic.clone()).or_default().push(page);
    }
    for (topic, topic_pages) in &by_topic {
        lines.push(format!("## {}", md_inline(topic)));
        lines.push(String::new());
        for page in topic_pages {
            lines.push(format!("- [[{}]]", md_inline(&page.title)));
        }
        lines.push(String::new());
    }

    let joined = lines.join("\n");
    format!("{}\n", joined.trim_end())
}

fn build_manifest(
    manifest: ItemRow,
    query: &ExportQuery,
    grouping: &str,
    item_count: u64,
    page_count: u64,
    by_source: &HashMap<String, u64>,
) -> Value {
    let mut map: Map<String, Value> = manifest.into_iter().collect();
    map.insert(
        "query".to_string(),
        json!({
            "sources": query.sources,
            "item_types": query.item_types,
            "since": query.since.map(iso_z),
            "until": query.until.map(iso_z),
            "include_deleted": query.include_deleted,
            "include_revisions": query.include_revisions,
            "include_raw": query.include_raw,
            "wiki_grouping": grouping,
        }),
    );
    map.insert(
        "counts".to_string(),
        json!({
            "items": item_count,
            "pages": page_count,
            "by_source": by_source,
        }),
    );
    Value::Object(map)
}

fn zip_err(e: zip::result::ZipError) -> DbsError {
    DbsError::Storage(format!("failed to write export: {e}"))
}

fn io_err(e: std::io::Error) -> DbsError {
    DbsError::Storage(format!("failed to write export: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    struct FakeSource {
        items: Vec<ItemRow>,
    }

    impl ExportSource for FakeSource {
        fn items(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(self.items.iter().cloned())
        }
        fn revisions(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(std::iter::empty())
        }
        fn media_blobs(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(std::iter::empty())
        }
        fn manifest(&self) -> ItemRow {
            ItemRow::new()
        }
    }

    fn row(fields: &[(&str, Value)]) -> ItemRow {
        let mut r = ItemRow::new();
        for (k, v) in fields {
            r.insert(k.to_string(), v.clone());
        }
        r
    }

    fn write_zip(
        source: &FakeSource,
        query: &ExportQuery,
    ) -> (zip::ZipArchive<Cursor<Vec<u8>>>, ExportResult) {
        let mut out: Vec<u8> = Vec::new();
        let result = WikiExporter.write(source, &mut out, query).unwrap();
        let archive = zip::ZipArchive::new(Cursor::new(out)).unwrap();
        (archive, result)
    }

    fn read_entry(archive: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> String {
        let mut file = archive.by_name(name).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn empty_result_set_still_writes_index_and_manifest() {
        let source = FakeSource { items: Vec::new() };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 0);
        assert_eq!(result.format, "wiki");
        let index = read_entry(&mut archive, "index.md");
        assert!(index.contains("dbs_page_count: 0"));
        let manifest = read_entry(&mut archive, "manifest.json");
        let parsed: Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(parsed["counts"]["items"], 0);
        assert_eq!(parsed["counts"]["pages"], 0);
    }

    #[test]
    fn topic_grouping_creates_source_and_tag_hub_pages() {
        let source = FakeSource {
            items: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("title", json!("Hello World")),
                ("url", json!("https://example.com")),
                ("tags", json!(["rust"])),
            ])],
        };
        let query = ExportQuery {
            wiki_grouping: "topic".to_string(),
            ..ExportQuery::default()
        };
        let (mut archive, result) = write_zip(&source, &query);
        assert_eq!(result.item_count, 1);
        let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
        assert!(names.contains(&"pages/source-raindrop.md".to_string()));
        assert!(names.contains(&"pages/tag-rust.md".to_string()));
        let source_page = read_entry(&mut archive, "pages/source-raindrop.md");
        assert!(source_page.contains("title: \"Source: raindrop\""));
        assert!(source_page.contains("[Hello World](https://example.com)"));
        let tag_page = read_entry(&mut archive, "pages/tag-rust.md");
        assert!(tag_page.contains("title: \"Tag: rust\""));
        assert!(tag_page.contains("[[Source: raindrop]]"));
    }

    #[test]
    fn item_grouping_creates_one_page_per_item() {
        let source = FakeSource {
            items: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("title", json!("Solo item")),
                ("body", json!("some body")),
            ])],
        };
        let query = ExportQuery {
            wiki_grouping: "item".to_string(),
            ..ExportQuery::default()
        };
        let (mut archive, result) = write_zip(&source, &query);
        assert_eq!(result.item_count, 1);
        let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
        assert!(names.contains(&"pages/solo-item.md".to_string()));
        assert!(!names.iter().any(|n| n.starts_with("pages/source-")));
        let page = read_entry(&mut archive, "pages/solo-item.md");
        assert!(page.contains("title: \"Solo item\""));
        assert!(page.contains("some body"));
    }

    #[test]
    fn multi_topic_grouping_creates_a_hub_page_per_distinct_tag() {
        let source = FakeSource {
            items: vec![
                row(&[
                    ("source", json!("raindrop")),
                    ("external_id", json!("e1")),
                    ("title", json!("one")),
                    ("tags", json!(["rust"])),
                ]),
                row(&[
                    ("source", json!("raindrop")),
                    ("external_id", json!("e2")),
                    ("title", json!("two")),
                    ("tags", json!(["backup"])),
                ]),
            ],
        };
        let query = ExportQuery {
            wiki_grouping: "topic".to_string(),
            ..ExportQuery::default()
        };
        let (archive, result) = write_zip(&source, &query);
        assert_eq!(result.item_count, 2);
        let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
        assert!(names.contains(&"pages/tag-rust.md".to_string()));
        assert!(names.contains(&"pages/tag-backup.md".to_string()));
        assert_eq!(result.extra.get("pages"), Some(&Value::from(3)));
    }

    #[test]
    fn filtered_subset_only_includes_matching_items() {
        let source = FakeSource {
            items: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("title", json!("kept")),
            ])],
        };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 1);
        let index = read_entry(&mut archive, "index.md");
        assert!(index.contains("[[Source: raindrop]]"));
    }

    #[test]
    fn unknown_grouping_is_a_config_error() {
        let source = FakeSource { items: Vec::new() };
        let query = ExportQuery {
            wiki_grouping: "bogus".to_string(),
            ..ExportQuery::default()
        };
        let mut out: Vec<u8> = Vec::new();
        match WikiExporter.write(&source, &mut out, &query) {
            Ok(_) => panic!("expected a config error"),
            Err(e) => assert!(e.to_string().contains("bogus")),
        }
    }

    #[test]
    fn exporter_metadata_matches_the_reference() {
        assert_eq!(WikiExporter.format(), "wiki");
        assert_eq!(WikiExporter.media_type(), "application/zip");
        assert_eq!(WikiExporter.file_ext(), ".zip");
    }
}

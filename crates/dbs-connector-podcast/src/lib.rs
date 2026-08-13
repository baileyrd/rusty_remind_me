//! Podcast connector: backs up episodes from RSS/Atom feeds you list
//! (issue #93). Mirrors `dbs.connectors.podcast` in
//! baileyrd/Daily-Backup-System.
//!
//! The source of truth is a plain list of feed URLs — the one format
//! every podcast app can export and no service can take away. Feeds
//! come from the `feeds` config list and/or an OPML file
//! (`opml_path`, the standard subscription-export format), merged and
//! deduplicated.
//!
//! Episode metadata is always stored (title, show notes, publish
//! date, enclosure URL as a `MediaRef`); `download_audio = true`
//! additionally downloads each enclosure into this source's download
//! folder. Audio is written to disk and referenced — never inlined
//! into the DB — because episodes are routinely 50-100 MB. Downloads
//! are idempotent (an existing non-empty file is skipped) and
//! best-effort (a dead enclosure never fails the run).
//!
//! Deletion detection is deliberately **disabled**: a podcast feed is
//! a rolling window over the newest N episodes, so an episode leaving
//! the feed is ordinary aging, not a deletion — sweeping against a
//! feed enumeration would eventually soft-delete every old episode
//! we backed up. Hence `supports_full_enumeration = false` and no
//! [`ReconcileMarker`][dbs_core::ReconcileMarker], ever; what this
//! connector has stored, it keeps. `supports_incremental` is also
//! false — feeds are small and carry no reliable delta parameter, so
//! every run simply re-reads each feed.
//!
//! One broken feed of many is logged and skipped so the healthy feeds
//! still make progress; only when *every* feed fails does the run
//! fail.
//!
//! **Not wired up:** same boundary as `dbs-connector-raindrop` (#85)
//! through `dbs-connector-pocketcasts` (#92) — this struct isn't
//! reachable from a real `dbs backup` run yet; the plugin registry's
//! run/stream bridge doesn't exist. Tested directly against the
//! `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use dbs_core::export_profile::ExportProfile;
use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, MediaRef, RunContext,
};

const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
const ITUNES_NS: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";

#[derive(Debug, Clone, Default)]
pub struct PodcastConfig {
    /// RSS/Atom feed URLs to back up.
    pub feeds: Vec<String>,
    /// Path to an OPML subscription export; its feed URLs are merged
    /// with `feeds` (deduplicated).
    pub opml_path: Option<String>,
    /// Also download each episode's enclosure into this source's
    /// download folder (referenced, never inlined into the DB).
    pub download_audio: bool,
    /// Per-feed cap on episodes handled per run (0 = no cap).
    pub max_episodes_per_feed: u32,
}

pub struct PodcastConnector {
    config: PodcastConfig,
    item_kinds: Vec<ItemKind>,
}

impl PodcastConnector {
    pub fn new(config: PodcastConfig) -> Self {
        Self {
            config,
            item_kinds: vec![ItemKind {
                name: "episode".to_string(),
                display_name: "Episode".to_string(),
                description: String::new(),
            }],
        }
    }

    /// `feeds` + the OPML file's outlines, deduplicated,
    /// order-preserving.
    fn resolve_feeds(&self) -> Result<Vec<String>, ConnectorError> {
        let mut urls = self.config.feeds.clone();
        if let Some(path) = &self.config.opml_path {
            if !std::path::Path::new(path).exists() {
                return Err(ConnectorError::Config(format!(
                    "OPML file not found: {path}"
                )));
            }
            let text = std::fs::read_to_string(path).map_err(|e| {
                ConnectorError::Config(format!("OPML file {path} could not be read: {e}"))
            })?;
            let doc = roxmltree::Document::parse(&text).map_err(|e| {
                ConnectorError::Config(format!("OPML file {path} is not valid XML: {e}"))
            })?;
            for outline in doc
                .descendants()
                .filter(|n| n.is_element() && n.tag_name().name() == "outline")
            {
                if let Some(xml_url) = outline.attribute("xmlUrl") {
                    let xml_url = xml_url.trim();
                    if !xml_url.is_empty() {
                        urls.push(xml_url.to_string());
                    }
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        let out: Vec<String> = urls
            .into_iter()
            .filter(|u| !u.is_empty() && seen.insert(u.clone()))
            .collect();
        if out.is_empty() {
            return Err(ConnectorError::Config(
                "the podcast connector needs at least one feed: set `feeds` and/or `opml_path`"
                    .to_string(),
            ));
        }
        Ok(out)
    }

    /// Fetch and parse one feed into (show_title, episodes). Any
    /// failure here — network or XML — is the caller's to log and
    /// skip; a single bad feed must never abort the whole run.
    fn fetch_feed(
        &self,
        http: &RefCell<ManagedHttpClient>,
        feed_url: &str,
    ) -> Result<(Option<String>, Vec<Episode>), ConnectorError> {
        let response = http
            .borrow_mut()
            .get(feed_url)
            .map_err(classify_feed_error)?;
        let text = response
            .text()
            .map_err(|e| ConnectorError::Transient(format!("invalid feed response: {e}")))?;
        parse_feed(&text)
            .map_err(|e| ConnectorError::Transient(format!("podcast: feed did not parse: {e}")))
    }

    fn to_item(
        &self,
        http: &RefCell<ManagedHttpClient>,
        download_dir: Option<&PathBuf>,
        feed_url: &str,
        ns: &str,
        show_title: Option<&str>,
        ep: &Episode,
    ) -> Option<BackupItem> {
        let raw = serde_json::json!({
            "guid": ep.guid,
            "title": ep.title,
            "link": ep.link,
            "description": ep.description,
            "published": ep.published,
            "enclosure_url": ep.enclosure_url,
            "enclosure_type": ep.enclosure_type,
            "enclosure_length": ep.enclosure_length,
            "itunes_duration": ep.itunes_duration,
            "itunes_episode": ep.itunes_episode,
            "feed_url": feed_url,
            "show_title": show_title,
        });
        let mut media = Vec::new();
        if let Some(enclosure) = &ep.enclosure_url {
            let local = if self.config.download_audio {
                download_enclosure(http, download_dir, show_title, ep, enclosure)
            } else {
                None
            };
            let filename = local
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string());
            let url = local
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| enclosure.clone());
            media.push(MediaRef {
                url,
                kind: "audio".to_string(),
                filename,
                mime: ep.enclosure_type.clone(),
                data: None,
            });
        }
        let mut item = BackupItem::new(format!("{ns}:{}", ep.guid), "episode", raw).ok()?;
        item.title = ep.title.clone();
        item.url = ep.link.clone().or_else(|| ep.enclosure_url.clone());
        item.body = ep.description.clone();
        item.tags = show_title.map(|s| vec![s.to_string()]).unwrap_or_default();
        item.created_at = ep.published;
        item.media = media;
        Some(item)
    }
}

/// Idempotent (existing non-empty file wins) and best-effort — a dead
/// enclosure must never fail the backup run.
fn download_enclosure(
    http: &RefCell<ManagedHttpClient>,
    download_dir: Option<&PathBuf>,
    show_title: Option<&str>,
    ep: &Episode,
    url: &str,
) -> Option<PathBuf> {
    let Some(download_dir) = download_dir else {
        eprintln!("podcast: no download_dir; skipping audio download");
        return None;
    };
    let folder = download_dir.join(slug(show_title.unwrap_or("podcast")));
    let target = folder.join(audio_filename(ep, url));
    if target.exists() && target.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Some(target);
    }
    let response = match http.borrow_mut().get(url) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("podcast: audio download failed for {url}: {e}");
            return None;
        }
    };
    let bytes = match response.bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("podcast: audio download failed for {url}: {e}");
            return None;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&folder) {
        eprintln!("podcast: audio download failed for {url}: {e}");
        return None;
    }
    if let Err(e) = std::fs::write(&target, &bytes) {
        eprintln!("podcast: audio download failed for {url}: {e}");
        return None;
    }
    Some(target)
}

#[derive(Debug, Clone, Default)]
struct Episode {
    guid: String,
    title: Option<String>,
    link: Option<String>,
    description: Option<String>,
    published: Option<DateTime<Utc>>,
    enclosure_url: Option<String>,
    enclosure_type: Option<String>,
    enclosure_length: Option<String>,
    itunes_duration: Option<String>,
    itunes_episode: Option<String>,
}

#[derive(Debug)]
enum ParseFeedError {
    Xml(roxmltree::Error),
    NotRssOrAtom(String),
}

impl std::fmt::Display for ParseFeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Xml(e) => write!(f, "{e}"),
            Self::NotRssOrAtom(tag) => write!(f, "not an RSS or Atom feed (root <{tag}>)"),
        }
    }
}

/// Pure; what the tests assert on directly.
fn parse_feed(text: &str) -> Result<(Option<String>, Vec<Episode>), ParseFeedError> {
    let doc = roxmltree::Document::parse(text).map_err(ParseFeedError::Xml)?;
    let root = doc.root_element();
    if root.tag_name().name() == "feed" && root.tag_name().namespace() == Some(ATOM_NS) {
        return Ok(parse_atom(root));
    }
    if root.tag_name().name() == "rss" {
        let channel = root
            .children()
            .find(|c| c.is_element() && c.tag_name().name() == "channel");
        if let Some(channel) = channel {
            return Ok(parse_rss(channel));
        }
    }
    Err(ParseFeedError::NotRssOrAtom(
        root.tag_name().name().to_string(),
    ))
}

fn parse_rss(channel: roxmltree::Node) -> (Option<String>, Vec<Episode>) {
    let show = direct_text(channel, None, "title");
    let mut episodes = Vec::new();
    for item in channel.children().filter(|c| {
        c.is_element() && c.tag_name().name() == "item" && c.tag_name().namespace().is_none()
    }) {
        let enclosure = item.children().find(|c| {
            c.is_element()
                && c.tag_name().name() == "enclosure"
                && c.tag_name().namespace().is_none()
        });
        let enclosure_url = enclosure
            .and_then(|e| e.attribute("url"))
            .map(str::to_string);
        let Some(guid) = direct_text(item, None, "guid")
            .or_else(|| enclosure_url.clone())
            .or_else(|| direct_text(item, None, "link"))
        else {
            continue; // nothing stable to identify the episode by
        };
        episodes.push(Episode {
            guid,
            title: direct_text(item, None, "title"),
            link: direct_text(item, None, "link"),
            description: direct_text(item, None, "description")
                .or_else(|| direct_text(item, Some(ITUNES_NS), "summary")),
            published: direct_text(item, None, "pubDate").and_then(|s| parse_pub_date(&s)),
            enclosure_url,
            enclosure_type: enclosure
                .and_then(|e| e.attribute("type"))
                .map(str::to_string),
            enclosure_length: enclosure
                .and_then(|e| e.attribute("length"))
                .map(str::to_string),
            itunes_duration: direct_text(item, Some(ITUNES_NS), "duration"),
            itunes_episode: direct_text(item, Some(ITUNES_NS), "episode"),
        });
    }
    (show, episodes)
}

fn parse_atom(feed: roxmltree::Node) -> (Option<String>, Vec<Episode>) {
    let show = direct_text(feed, Some(ATOM_NS), "title");
    let mut episodes = Vec::new();
    for entry in feed.children().filter(|c| {
        c.is_element()
            && c.tag_name().name() == "entry"
            && c.tag_name().namespace() == Some(ATOM_NS)
    }) {
        let mut link = None;
        let mut enclosure_url = None;
        let mut enclosure_type = None;
        for ln in entry.children().filter(|c| {
            c.is_element()
                && c.tag_name().name() == "link"
                && c.tag_name().namespace() == Some(ATOM_NS)
        }) {
            if ln.attribute("rel") == Some("enclosure") {
                enclosure_url = ln.attribute("href").map(str::to_string);
                enclosure_type = ln.attribute("type").map(str::to_string);
            } else if link.is_none() {
                link = ln.attribute("href").map(str::to_string);
            }
        }
        let Some(guid) = direct_text(entry, Some(ATOM_NS), "id")
            .or_else(|| enclosure_url.clone())
            .or_else(|| link.clone())
        else {
            continue;
        };
        let published = direct_text(entry, Some(ATOM_NS), "published")
            .or_else(|| direct_text(entry, Some(ATOM_NS), "updated"));
        episodes.push(Episode {
            guid,
            title: direct_text(entry, Some(ATOM_NS), "title"),
            link,
            description: direct_text(entry, Some(ATOM_NS), "summary")
                .or_else(|| direct_text(entry, Some(ATOM_NS), "content")),
            published: published.and_then(|s| parse_iso(Some(&s))),
            enclosure_url,
            enclosure_type,
            enclosure_length: None,
            itunes_duration: direct_text(entry, Some(ITUNES_NS), "duration"),
            itunes_episode: direct_text(entry, Some(ITUNES_NS), "episode"),
        });
    }
    (show, episodes)
}

/// The first direct child element named `name` in namespace `ns`
/// (`None` for no namespace), its text trimmed — `None` if missing or
/// empty, mirroring the reference's `_text` helper.
fn direct_text(node: roxmltree::Node, ns: Option<&str>, name: &str) -> Option<String> {
    let child = node.children().find(|c| {
        c.is_element() && c.tag_name().name() == name && c.tag_name().namespace() == ns
    })?;
    let text: String = child.children().filter_map(|t| t.text()).collect();
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// RFC 2822 (the common `pubDate` format) with an ISO-8601 fallback —
/// some feeds put ISO stamps in `pubDate` despite the spec.
fn parse_pub_date(value: &str) -> Option<DateTime<Utc>> {
    if value.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc2822(value) {
        return Some(dt.with_timezone(&Utc));
    }
    parse_iso(Some(value))
}

/// A stable short per-feed namespace so guids can't collide across
/// feeds. Not required to match the reference's SHA-1-based value bit
/// for bit — these are two independent datastores — only to be
/// stable and distinct per URL.
fn feed_ns(feed_url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    feed_url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())[..12].to_string()
}

fn slug(name: &str) -> String {
    let mut result = String::new();
    let mut last_was_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            result.push(c);
            last_was_dash = false;
        } else if !last_was_dash {
            result.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = result.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "podcast".to_string()
    } else {
        trimmed.to_string()
    }
}

fn audio_filename(ep: &Episode, url: &str) -> String {
    let base_src = ep
        .title
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&ep.guid);
    let base: String = slug(base_src).chars().take(120).collect();
    let tail = url.split('?').next().unwrap_or(url);
    let tail = tail.rsplit('/').next().unwrap_or(tail);
    let ext = match tail.rfind('.') {
        Some(idx) => format!(".{}", &tail[idx + 1..]),
        None => ".mp3".to_string(),
    };
    format!("{base}{ext}")
}

/// 5xx/timeouts already surface as `Transient`/`RateLimited` from the
/// managed client after its retries; a 4xx here is just this feed's
/// problem, not a fatal auth error — feeds are public and this
/// connector has no token to reject.
fn classify_feed_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status) => ConnectorError::Transient(format!("podcast feed error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
    }
}

impl Connector for PodcastConnector {
    fn type_name(&self) -> &str {
        "podcast"
    }

    fn display_name(&self) -> &str {
        "Podcasts (RSS)"
    }

    fn description(&self) -> &str {
        "Backs up podcast episodes from RSS/Atom feeds (and OPML exports)."
    }

    fn setup_hint(&self) -> &str {
        "List feed URLs under `feeds`, or point `opml_path` at a subscription export from your \
         podcast app. No account or token needed."
    }

    fn wants_managed_http(&self) -> bool {
        true
    }

    fn item_kinds(&self) -> &[ItemKind] {
        &self.item_kinds
    }

    fn export_profile(&self) -> Option<ExportProfile> {
        Some(ExportProfile {
            group_by: vec!["feed_title".to_string()],
            body_from: vec!["summary".to_string()],
            ..Default::default()
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // No reliable delta; feeds are small.
            supports_incremental: false,
            // Rolling windows — never sweep (see module docstring).
            supports_full_enumeration: false,
            supports_native_deletes: false,
            produces_media: true,
            media_inline: false,
            items_mutable: true, // show notes get edited upstream
            requires_auth: false,
            supports_rate_limit_backoff: true,
            paginated: false,
            ..Capabilities::default()
        }
    }

    /// Reads `feeds` from this source's `[sources.NAME]` config
    /// (ADR-0002) — unlike every other connector wired up so far, this
    /// one has no separate "host" field at all; the feed URL list *is*
    /// the entire input, so without this there is nothing to back up.
    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = options.get("feeds") {
            let feeds = v.as_array().ok_or_else(|| {
                ConnectorError::Config(format!(
                    "sources.<name>.feeds must be an array of strings, got {v}"
                ))
            })?;
            self.config.feeds = feeds
                .iter()
                .map(|f| {
                    f.as_str().map(str::to_string).ok_or_else(|| {
                        ConnectorError::Config(format!(
                            "sources.<name>.feeds entries must be strings, got {f}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
        Ok(())
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        let Some(http) = ctx.http.as_ref() else {
            out.push(Err(ConnectorError::Config(
                "Podcast connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        let feeds = match self.resolve_feeds() {
            Ok(f) => f,
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let mut failures = Vec::new();
        let mut done = 0usize;
        for feed_url in &feeds {
            match self.fetch_feed(http, feed_url) {
                Ok((show_title, episodes)) => {
                    let episodes: Vec<Episode> = if self.config.max_episodes_per_feed > 0 {
                        episodes
                            .into_iter()
                            .take(self.config.max_episodes_per_feed as usize)
                            .collect()
                    } else {
                        episodes
                    };
                    let ns = feed_ns(feed_url);
                    for ep in &episodes {
                        let Some(item) = self.to_item(
                            http,
                            ctx.download_dir.as_ref(),
                            feed_url,
                            &ns,
                            show_title.as_deref(),
                            ep,
                        ) else {
                            continue;
                        };
                        out.push(Ok(FetchEvent::Item(item)));
                    }
                    done += 1;
                    out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                        cursor: Cursor {
                            value: serde_json::json!({"feeds_done": done}),
                        },
                        note: format!(
                            "after {}",
                            show_title.as_deref().unwrap_or(feed_url.as_str())
                        ),
                    })));
                }
                Err(e) => {
                    eprintln!("podcast: feed {feed_url} failed: {e}");
                    failures.push(feed_url.clone());
                }
            }
        }

        if !failures.is_empty() && done == 0 {
            out.push(Err(ConnectorError::Transient(format!(
                "podcast: every feed failed ({}): {}",
                failures.len(),
                failures.join(", ")
            ))));
        }

        Box::new(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(http: ManagedHttpClient, download_dir: Option<PathBuf>) -> RunContext {
        RunContext {
            source_id: 1,
            source_name: "podcast".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec![]),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir,
            items_failed: 0,
            cancel: None,
            http: Some(RefCell::new(http)),
        }
    }

    fn no_sleep_client() -> ManagedHttpClient {
        ManagedHttpClient::with_sleep(reqwest::blocking::Client::new(), |_| {})
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-connector-podcast-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const RSS_FEED: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <title>A Show</title>
    <item>
      <guid>ep-1</guid>
      <title>Episode One</title>
      <link>https://example.com/ep1</link>
      <description>Show notes for episode one</description>
      <pubDate>Wed, 01 Jun 2024 12:00:00 +0000</pubDate>
      <enclosure url="https://example.com/ep1.mp3" type="audio/mpeg" length="12345"/>
      <itunes:duration>30:00</itunes:duration>
      <itunes:episode>1</itunes:episode>
    </item>
  </channel>
</rss>"#;

    const ATOM_FEED: &str = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>An Atom Show</title>
  <entry>
    <id>atom-ep-1</id>
    <title>Atom Episode One</title>
    <link href="https://example.com/atom-ep1"/>
    <link rel="enclosure" href="https://example.com/atom-ep1.mp3" type="audio/mpeg"/>
    <summary>Atom show notes</summary>
    <published>2024-06-01T12:00:00Z</published>
  </entry>
</feed>"#;

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<Result<FetchEvent, ConnectorError>> {
        iter.collect()
    }

    #[test]
    fn configure_applies_a_feeds_array_from_options() {
        let mut connector = PodcastConnector::new(PodcastConfig::default());
        assert!(connector.config.feeds.is_empty());
        let options = HashMap::from([(
            "feeds".to_string(),
            serde_json::json!(["https://example.com/a.xml", "https://example.com/b.xml"]),
        )]);
        connector.configure(&options).unwrap();
        assert_eq!(
            connector.config.feeds,
            vec![
                "https://example.com/a.xml".to_string(),
                "https://example.com/b.xml".to_string(),
            ]
        );
    }

    #[test]
    fn configure_rejects_a_non_array_feeds_value() {
        let mut connector = PodcastConnector::new(PodcastConfig::default());
        let options = HashMap::from([("feeds".to_string(), serde_json::json!("not-an-array"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_feeds_array_with_a_non_string_entry() {
        let mut connector = PodcastConnector::new(PodcastConfig::default());
        let options = HashMap::from([("feeds".to_string(), serde_json::json!([1, 2]))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec!["https://example.com/feed.xml".to_string()],
            ..Default::default()
        });
        let ctx = RunContext {
            source_id: 1,
            source_name: "podcast".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec![]),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
            http: None,
        };
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_with_no_feeds_configured_is_a_config_error() {
        let mut connector = PodcastConnector::new(PodcastConfig::default());
        let ctx = ctx_with(no_sleep_client(), None);
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn a_single_rss_feed_is_parsed_into_episodes() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_body(RSS_FEED)
            .create();

        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec![format!("{}/feed.xml", server.url())],
            ..Default::default()
        });
        let ctx = ctx_with(no_sleep_client(), None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 1, "{evs:?}");
        assert_eq!(items[0].title.as_deref(), Some("Episode One"));
        assert_eq!(items[0].tags, vec!["A Show".to_string()]);
        assert_eq!(items[0].url.as_deref(), Some("https://example.com/ep1"));
        assert_eq!(items[0].media.len(), 1);
        assert_eq!(items[0].media[0].url, "https://example.com/ep1.mp3");
        assert!(!evs
            .iter()
            .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))));
    }

    #[test]
    fn an_atom_feed_is_parsed_into_episodes() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/atom.xml")
            .with_status(200)
            .with_body(ATOM_FEED)
            .create();

        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec![format!("{}/atom.xml", server.url())],
            ..Default::default()
        });
        let ctx = ctx_with(no_sleep_client(), None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 1, "{evs:?}");
        assert_eq!(items[0].title.as_deref(), Some("Atom Episode One"));
        assert_eq!(items[0].media[0].url, "https://example.com/atom-ep1.mp3");
    }

    #[test]
    fn opml_feeds_are_merged_and_deduplicated_with_configured_feeds() {
        let mut server = mockito::Server::new();
        let feed_url = format!("{}/feed.xml", server.url());
        let _m = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_body(RSS_FEED)
            .create();

        let dir = temp_dir("opml");
        let opml_path = dir.join("subscriptions.opml");
        std::fs::write(
            &opml_path,
            format!(
                r#"<opml version="1.0"><body><outline text="feeds"><outline xmlUrl="{feed_url}"/></outline></body></opml>"#
            ),
        )
        .unwrap();

        // The OPML file references the SAME feed already in `feeds`,
        // proving dedup, not just merge.
        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec![feed_url],
            opml_path: Some(opml_path.to_string_lossy().to_string()),
            ..Default::default()
        });
        let ctx = ctx_with(no_sleep_client(), None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let items: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, FetchEvent::Item(_)))
            .collect();
        assert_eq!(items.len(), 1, "{evs:?}");
    }

    #[test]
    fn one_broken_feed_of_many_is_skipped_and_healthy_feeds_still_yield_items() {
        let mut server = mockito::Server::new();
        let _m_good = server
            .mock("GET", "/good.xml")
            .with_status(200)
            .with_body(RSS_FEED)
            .create();
        let _m_bad = server
            .mock("GET", "/bad.xml")
            .with_status(500)
            .with_body("boom")
            .create();

        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec![
                format!("{}/bad.xml", server.url()),
                format!("{}/good.xml", server.url()),
            ],
            ..Default::default()
        });
        let ctx = ctx_with(no_sleep_client(), None);
        let result = events(connector.fetch(&ctx));
        assert!(!result.iter().any(|r| r.is_err()), "{result:?}");
        let items: Vec<_> = result
            .iter()
            .filter(|r| matches!(r, Ok(FetchEvent::Item(_))))
            .collect();
        assert_eq!(items.len(), 1, "{result:?}");
    }

    #[test]
    fn every_feed_failing_is_a_transient_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/bad.xml")
            .with_status(500)
            .with_body("boom")
            .create();

        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec![format!("{}/bad.xml", server.url())],
            ..Default::default()
        });
        let ctx = ctx_with(no_sleep_client(), None);
        let result = events(connector.fetch(&ctx));
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Transient(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn download_audio_downloads_the_enclosure_into_the_download_dir() {
        let mut server = mockito::Server::new();
        let _m_feed = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_body(RSS_FEED)
            .create();
        let _m_audio = server
            .mock("GET", "/ep1.mp3")
            .with_status(200)
            .with_body(b"fake-audio-bytes".as_slice())
            .create();
        // The RSS body's enclosure URL is a fixed example.com host, so
        // point the connector at a feed that instead references this
        // mock server's own audio path.
        let feed_with_local_enclosure = RSS_FEED.replace(
            "https://example.com/ep1.mp3",
            &format!("{}/ep1.mp3", server.url()),
        );
        let _m_feed2 = server
            .mock("GET", "/feed2.xml")
            .with_status(200)
            .with_body(feed_with_local_enclosure)
            .create();

        let dir = temp_dir("download");
        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec![format!("{}/feed2.xml", server.url())],
            download_audio: true,
            ..Default::default()
        });
        let ctx = ctx_with(no_sleep_client(), Some(dir.clone()));
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        let local_path = std::path::Path::new(&item.media[0].url);
        assert!(local_path.exists(), "{local_path:?} should exist");
        assert_eq!(std::fs::read(local_path).unwrap(), b"fake-audio-bytes");
        assert!(local_path.starts_with(&dir));
    }

    #[test]
    fn max_episodes_per_feed_caps_the_episode_count() {
        let two_episode_feed = RSS_FEED.replace(
            "</channel>",
            r#"<item><guid>ep-2</guid><title>Episode Two</title>
               <pubDate>Thu, 02 Jun 2024 12:00:00 +0000</pubDate></item></channel>"#,
        );
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/feed.xml")
            .with_status(200)
            .with_body(two_episode_feed)
            .create();

        let mut connector = PodcastConnector::new(PodcastConfig {
            feeds: vec![format!("{}/feed.xml", server.url())],
            max_episodes_per_feed: 1,
            ..Default::default()
        });
        let ctx = ctx_with(no_sleep_client(), None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let items: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, FetchEvent::Item(_)))
            .collect();
        assert_eq!(items.len(), 1, "{evs:?}");
    }

    #[test]
    fn parse_feed_rejects_a_non_rss_atom_root() {
        let result = parse_feed("<not-a-feed/>");
        assert!(matches!(result, Err(ParseFeedError::NotRssOrAtom(_))));
    }

    #[test]
    fn an_episode_with_no_guid_link_or_enclosure_is_skipped() {
        let feed = r#"<rss version="2.0"><channel><title>S</title>
            <item><title>No identity</title></item></channel></rss>"#;
        let (_, episodes) = parse_feed(feed).unwrap();
        assert!(episodes.is_empty());
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = PodcastConnector::new(PodcastConfig::default());
        assert_eq!(connector.type_name(), "podcast");
        assert!(connector.secret_keys().is_empty());
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 1);
        assert!(!connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(!connector.capabilities().supports_full_enumeration);
        assert!(connector.capabilities().produces_media);
        assert!(connector.export_profile().is_some());
    }
}

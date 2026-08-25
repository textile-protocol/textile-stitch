// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The published bot versions an operator can roll back to.
//!
//! [`super::updates`] answers "is there something newer?". This answers "what
//! else has been published, and when?" — the list behind Tools → Roll back.
//!
//! The registry supplies the tags; GitHub supplies the order. A `sha-*` tag *is*
//! the commit's short sha, so the commits API — anonymous, unlike the packages
//! API, which wants a token even for a public image — says which build is newer
//! and what it changed. The registry's own tag list can't: the Distribution spec
//! orders it lexically, and lexical order over `sha-<hex>` is in effect random.
//! Where that lookup can't place a build — a private repo, a rate limit, a
//! registry that isn't GHCR, or a tag built off another branch — rolling back to
//! it still works, because a target is just a tag. What's lost is the order, so
//! the reply grades it and the picker only calls a row the newest when every row
//! was placed. See [`VersionOrdering`].
//!
//! Only immutable tags are offered: a rollback has to name one exact build, and
//! a channel tag like `latest` moves to whatever is published next, which would
//! quietly undo the pin.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
#[cfg(not(test))]
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use serde::Serialize;

use super::updates::{
    distribution_api_host, is_behind, is_content_digest_ref, parse_image_ref, registry_token,
};

/// How many published versions the rollback picker offers.
pub const ROLLBACK_CHOICES: usize = 10;

/// Reused for the same reason as the update check's: the detail page asks on
/// every visit, and the tag list plus ten manifest lookups is not something to
/// hand a registry on a poll.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Tags per page. The Distribution API caps this server-side, so pagination is
/// still followed; a large page just means one request for a normal repository.
const PAGE_SIZE: usize = 1000;

/// Pagination stops here. A repository with more tags than this has its newest
/// ones at the end anyway, and an unbounded follow is a way to hang a request.
const MAX_PAGES: usize = 20;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Failed GitHub listings and "this SHA is not in the index yet" (container
/// workflow published `sha-*` before `release.yml` cut the `v*` tag) retry
/// faster than a complete hit. The full [`CACHE_TTL`] would pin a just-released
/// bot to the SHA fallback for 15 minutes.
const RETRY_TTL: Duration = Duration::from_secs(30);

/// Generation token for the refresh currently running for a cache key.
#[cfg_attr(test, allow(dead_code))]
static RELEASE_REFRESH_GEN: AtomicU64 = AtomicU64::new(1);

/// One published build, as the rollback picker shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedVersion {
    /// Registry tag, e.g. `sha-14cd877`.
    pub tag: String,
    /// Full image reference a rollback would recreate the bot on.
    pub image: String,
    /// Content digest of the tag, when the registry answered.
    pub digest: Option<String>,
    /// Commit timestamp (RFC 3339), when GitHub could attribute the tag.
    pub published_at: Option<String>,
    /// Commit subject for that build. Best effort, same as `published_at`.
    pub subject: Option<String>,
    /// GitHub release that names this commit, e.g. `v0.1.226`. The registry tag
    /// stays `sha-*`; this is what the picker should show.
    pub version: Option<String>,
}

/// True when `tag` names exactly one build forever.
///
/// `sha-<short sha>`, and only that: it's what `docker/metadata-action` publishes
/// per commit, and it's the one tag shape this ranking can place, because the tag
/// *is* the commit. Everything else — `latest`, `main`, a branch name — is a
/// channel that moves.
///
/// A release tag like `v1.2.3` would be immutable too, and an earlier draft
/// accepted it. It's refused because nothing here could order it: the tag names
/// a release, not a commit, so [`recent_tags`] would rank every one of them
/// behind every `sha-*` build and quietly drop them off a ten-row list. Better to
/// refuse a shape than to offer it and mis-sort it. This pipeline publishes no
/// such tag today (see `.github/workflows/container.yml`); whoever adds one
/// teaches the ranking to resolve it in the same change.
pub fn is_immutable_tag(tag: &str) -> bool {
    match tag.strip_prefix("sha-") {
        Some(hex) => (7..=40).contains(&hex.len()) && hex.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

/// Refuse a rollback target that isn't one exact build, with the reason.
///
/// The repository a rollback pulls from is the panel's own configured one, so a
/// caller only ever chooses the tag — this is the whole of what it may choose.
pub fn check_rollback_tag(tag: &str) -> Result<(), String> {
    if tag.trim().is_empty() {
        return Err("no version was chosen".into());
    }
    if is_immutable_tag(tag) {
        return Ok(());
    }
    Err(format!(
        "{tag} isn't a build tag. A rollback pins the bot to one exact published build, which \
         is what a sha-… tag names; a channel tag like latest or a branch name moves to \
         whatever is published next, so the pin would undo itself. Use Update to go to the \
         newest build."
    ))
}

/// The image reference a rollback to `tag` would use.
///
/// Built from the panel's configured bot image, never from the request: the
/// operator picks a version, not a registry.
pub fn rollback_image(configured: &str, tag: &str) -> Option<String> {
    let parsed = parse_image_ref(configured);
    let registry = parsed.registry.as_deref()?;
    Some(format!("{registry}/{}:{tag}", parsed.repository))
}

/// The newest `limit` immutable tags, newest first.
///
/// Ordered by the commit behind each tag, not by the registry's tag list. The
/// Distribution API doesn't promise push order — the spec says lexical, and a
/// lexical list of `sha-<hex>` tags is in effect random, which would put an
/// arbitrary old build at the top and drop genuinely recent ones off the end.
/// (GHCR happens to append, which is why the tail *looks* right there.)
///
/// A tag GitHub can't place — a build older than the commit window, or one from
/// a branch that isn't the default — ranks after every tag it can, in reverse
/// registry order. Those fill leftover slots rather than displacing a known
/// release, and with no commit data at all the whole list falls back to that
/// registry order.
fn recent_tags(
    tags: &[String],
    commits: &HashMap<String, CommitInfo>,
    limit: usize,
) -> Vec<String> {
    let mut ranked: Vec<(Option<usize>, usize, &String)> = tags
        .iter()
        .enumerate()
        .filter(|(_, tag)| is_immutable_tag(tag))
        .map(|(index, tag)| (commits.get(short_sha(tag)).map(|c| c.position), index, tag))
        .collect();
    ranked.sort_by(|a, b| match (a.0, b.0) {
        // Position 0 is the newest commit on the default branch.
        (Some(a_pos), Some(b_pos)) => a_pos.cmp(&b_pos),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        // Neither is attributable: the registry's own order is all there is, and
        // its tail is the newest wherever the registry appends.
        (None, None) => b.1.cmp(&a.1),
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, tag)| tag.clone())
        .collect()
}

/// What a finished list's order actually means.
///
/// The picker may say "newest first" only when that's true of the whole list.
/// One unplaced row is enough to sink the claim: it sorts last by construction,
/// but nothing knows when it was built, so it could be newer than every row
/// above it. There is still a list either way — a rollback target is a tag, and
/// pulling one works regardless — it just isn't a ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VersionOrdering {
    /// Every row placed by the commit behind its tag. Provably newest first.
    Commit,
    /// Some rows placed, some not. The placed ones are in order and the rest are
    /// appended after them, so the list is still useful — but an unplaced build
    /// (one off another branch, or older than the commit window) could be newer
    /// than anything above it, so no row may be called the newest.
    Partial,
    /// Nothing placed. The registry's own tag order, and the Distribution spec
    /// orders tags lexically — lexical order over `sha-<hex>` says nothing about
    /// age. GHCR's happens to be push order, but that isn't a promise to lean on.
    Registry,
}

/// Which of the three a finished list got.
///
/// Read off the rows, not off "did the commit lookup return anything": a lookup
/// can succeed and still place none of these tags, which leaves an order just as
/// unjustified as no lookup at all.
pub fn ordering_of(versions: &[PublishedVersion]) -> VersionOrdering {
    let placed = versions.iter().filter(|v| v.published_at.is_some()).count();
    match placed {
        0 => VersionOrdering::Registry,
        n if n == versions.len() => VersionOrdering::Commit,
        _ => VersionOrdering::Partial,
    }
}

/// Whether `version` is the build a container running `local_digests` (and
/// named `current_image`) is on.
///
/// Two ways, because both cases are normal. A bot pinned by an earlier rollback
/// or by `STITCH_PANEL_BOT_IMAGE` names the tag outright. A bot on `:latest`
/// names a channel, so only the digest says which build that channel resolved
/// to when it was pulled.
pub fn is_current(
    version: &PublishedVersion,
    local_digests: &[String],
    current_image: Option<&str>,
) -> bool {
    if current_image == Some(version.image.as_str()) {
        return true;
    }
    match version.digest.as_deref() {
        // `is_behind` reads an empty local list as "can't tell", which must not
        // become "yes, that's the one you're running" for every row.
        Some(digest) if !local_digests.is_empty() => !is_behind(local_digests, digest),
        _ => false,
    }
}

struct CacheEntry {
    at: Instant,
    image: String,
    versions: Vec<PublishedVersion>,
}

static CACHE: Mutex<Option<CacheEntry>> = Mutex::new(None);

#[derive(Clone)]
#[cfg_attr(test, allow(dead_code))]
struct ReleaseCacheEntry {
    at: Instant,
    repo: String,
    /// Short-sha prefix → `v0.1.226`.
    by_sha: HashMap<String, String>,
    /// False when the GitHub listing errored mid-page. Empty-and-ok is a real
    /// repo with no `v*` tags; empty-and-failed should be retried soon.
    fetched_ok: bool,
    /// Complete fetches that still lacked the SHA we were looking for. Caps
    /// the 30s retry so a never-released commit doesn't scan GitHub forever.
    miss_streak: u32,
}

static RELEASE_CACHE: LazyLock<Mutex<HashMap<String, ReleaseCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RELEASE_INFLIGHT: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
thread_local! {
    static TEST_RELEASES: std::cell::RefCell<TestReleaseIndex> =
        std::cell::RefCell::new(TestReleaseIndex::default());
}

#[cfg(test)]
#[derive(Clone, Default)]
struct TestReleaseIndex {
    /// Used when no per-repo override is set — the original single-map helper.
    any: Option<HashMap<String, String>>,
    by_repo: HashMap<String, HashMap<String, String>>,
}

#[cfg(test)]
impl TestReleaseIndex {
    fn for_repo(&self, repo: &str) -> Option<HashMap<String, String>> {
        self.by_repo.get(repo).cloned().or_else(|| self.any.clone())
    }
}

pub fn clear_cache() {
    *CACHE.lock().unwrap() = None;
    RELEASE_CACHE.lock().unwrap().clear();
    RELEASE_INFLIGHT.lock().unwrap().clear();
}

/// The `limit` most recently published versions of `image`'s repository, newest
/// first.
///
/// Errors are for the caller to show as "couldn't list versions", not to fail a
/// page on: an offline panel or a private registry still has a working detail
/// screen, it just can't offer a rollback.
pub async fn list_published(image: &str, limit: usize) -> Result<Vec<PublishedVersion>> {
    if let Ok(guard) = CACHE.lock() {
        if let Some(entry) = guard.as_ref() {
            if entry.image == image && entry.at.elapsed() < CACHE_TTL {
                return Ok(entry.versions.clone());
            }
        }
    }

    let parsed = parse_image_ref(image);
    let registry = parsed.registry.as_deref().context(
        "this bot image has no registry path, so published versions can't be listed — point \
         STITCH_PANEL_BOT_IMAGE at ghcr.io/textile-protocol/textile-stitch",
    )?;
    let repo = parsed.repository.as_str();

    let client = reqwest::Client::builder()
        .user_agent(concat!("stitch-panel/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building the registry HTTP client")?;
    let token = registry_token(&client, registry, repo).await?;
    let api_host = distribution_api_host(registry);

    let tags = fetch_tags(&client, api_host, repo, token.as_deref()).await?;
    // Before the tags are cut down to ten, not after: which ten are the newest
    // is decided from the commit order, so it has to be known first.
    let (commits, releases) = tokio::join!(
        commit_metadata(&client, registry, repo),
        published_release_index(&client, registry, repo),
    );
    let recent = recent_tags(&tags, &commits, limit);
    let digests = fetch_digests(&client, api_host, repo, token.as_deref(), &recent).await;

    let versions: Vec<PublishedVersion> = recent
        .into_iter()
        .zip(digests)
        .map(|(tag, digest)| {
            let commit = commits.get(short_sha(&tag));
            PublishedVersion {
                image: format!("{registry}/{repo}:{tag}"),
                digest,
                published_at: commit.map(|c| c.date.clone()),
                subject: commit.map(|c| c.subject.clone()),
                version: lookup_release(&releases, short_sha(&tag)),
                tag,
            }
        })
        .collect();

    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(CacheEntry {
            at: Instant::now(),
            image: image.to_string(),
            versions: versions.clone(),
        });
    }
    Ok(versions)
}

/// Every tag in the repository, in the registry's push order.
async fn fetch_tags(
    client: &reqwest::Client,
    api_host: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct TagsBody {
        #[serde(default)]
        tags: Vec<String>,
    }

    let mut url = format!("https://{api_host}/v2/{repo}/tags/list?n={PAGE_SIZE}");
    let mut tags = Vec::new();
    for _ in 0..MAX_PAGES {
        let mut req = client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req
            .send()
            .await
            .with_context(|| format!("listing tags for {repo}"))?
            .error_for_status()
            .with_context(|| format!("registry rejected the tag list for {repo}"))?;
        let next = res
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .and_then(next_page_url)
            .map(|path| format!("https://{api_host}{path}"));
        let body: TagsBody = res
            .json()
            .await
            .with_context(|| format!("reading the tag list for {repo}"))?;
        tags.extend(body.tags);
        match next {
            Some(u) => url = u,
            None => break,
        }
    }
    Ok(tags)
}

/// The `rel="next"` path out of a Distribution `Link` header.
fn next_page_url(link: &str) -> Option<&str> {
    link.split(',')
        .filter(|part| part.contains("rel=\"next\"") || part.contains("rel=next"))
        .find_map(|part| {
            let start = part.find('<')? + 1;
            let end = part[start..].find('>')? + start;
            Some(part[start..end].trim())
        })
        .filter(|path| path.starts_with('/'))
}

/// Content digest per tag, `None` where the registry didn't answer.
///
/// One HEAD each, in parallel: a failure here costs the "currently running"
/// marker on that row, not the row.
async fn fetch_digests(
    client: &reqwest::Client,
    api_host: &str,
    repo: &str,
    token: Option<&str>,
    tags: &[String],
) -> Vec<Option<String>> {
    join_all(tags.iter().map(|tag| async move {
        let mut req = client
            .head(format!("https://{api_host}/v2/{repo}/manifests/{tag}"))
            .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.ok()?.error_for_status().ok()?;
        res.headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }))
    .await
}

const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

/// The 7-char commit prefix a `sha-*` tag carries, or the tag itself.
fn short_sha(tag: &str) -> &str {
    tag.strip_prefix("sha-").unwrap_or(tag)
}

/// What GitHub knows about the build behind a tag.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitInfo {
    /// Index in the default branch's newest-first commit list, so 0 is the
    /// newest. Ordering keys off this rather than [`Self::date`]: it needs no
    /// date parsing (no crate in this tree does it) and makes no assumption
    /// about the offset GitHub formats a commit timestamp with.
    position: usize,
    /// RFC 3339, for display only.
    date: String,
    subject: String,
}

/// Commit position, date and subject keyed by short sha, for GHCR images
/// published from a public GitHub repository of the same name.
///
/// Anonymous and best effort: any failure (rate limit, private repo, a registry
/// that isn't GHCR, an image name that isn't a repository name) yields an empty
/// map, a list ordered the way the registry returned it, and bare tags.
///
/// The commits endpoint answers newest first, which is the order
/// [`recent_tags`] ranks by.
async fn commit_metadata(
    client: &reqwest::Client,
    registry: &str,
    repo: &str,
) -> HashMap<String, CommitInfo> {
    #[derive(serde::Deserialize)]
    struct Commit {
        sha: String,
        commit: CommitDetail,
    }
    #[derive(serde::Deserialize)]
    struct CommitDetail {
        message: String,
        author: CommitAuthor,
    }
    #[derive(serde::Deserialize)]
    struct CommitAuthor {
        date: String,
    }

    if registry != "ghcr.io" || repo.matches('/').count() != 1 {
        return HashMap::new();
    }
    let url = format!("https://api.github.com/repos/{repo}/commits?per_page=100");
    let Ok(res) = client.get(&url).send().await else {
        return HashMap::new();
    };
    let Ok(res) = res.error_for_status() else {
        return HashMap::new();
    };
    let Ok(commits) = res.json::<Vec<Commit>>().await else {
        return HashMap::new();
    };
    index_commits(
        commits
            .into_iter()
            .map(|c| (c.sha, c.commit.author.date, subject_line(&c.commit.message))),
    )
}

/// A commit's subject: the first line of its message.
fn subject_line(message: &str) -> String {
    message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// GitHub `v*` tags keyed by short-sha, for turning a `sha-*` image into the
/// release number operators actually recognize.
///
/// Returns the last cached map immediately — never waits on GitHub. A cold or
/// expired cache (or one that doesn't yet contain `needed_sha`) kicks a
/// background refresh; the caller already has Docker labels as a fallback, so
/// the fleet page stays up when GH is slow. Same best-effort rules as
/// [`commit_metadata`]: a private repo, a rate limit, or a registry that isn't
/// GHCR yields an empty map and the UI keeps showing the image ref.
pub fn release_index(image: &str, needed_sha: Option<&str>) -> HashMap<String, String> {
    let parsed = parse_image_ref(image);
    let repo = parsed.repository;

    #[cfg(test)]
    {
        let _ = needed_sha;
        let override_index = TEST_RELEASES.with(|cell| cell.borrow().for_repo(&repo));
        if let Some(index) = override_index {
            return index;
        }
        return HashMap::new();
    }

    #[cfg(not(test))]
    {
        let Some(registry) = parsed.registry.clone() else {
            return HashMap::new();
        };
        let key = release_cache_key(&registry, &repo);
        let cached = cached_releases(&key);
        if cached
            .as_ref()
            .is_none_or(|entry| should_refresh_releases(entry, needed_sha))
        {
            spawn_release_refresh(registry, repo, needed_sha.map(str::to_string));
        }
        cached.map(|entry| entry.by_sha).unwrap_or_default()
    }
}

/// How long a one-shot detail/action read will wait for the first GitHub fill.
///
/// The fleet list never waits — it polls. Bot detail loads once, so a cold
/// cache would otherwise pin the SHA fallback for the whole visit.
pub const DETAIL_RELEASE_WAIT: Duration = Duration::from_secs(2);

/// Same as [`release_index`], but if this repo has never been cached, wait up
/// to `wait` for the background fill so a one-shot page can show the version.
pub async fn release_index_await(
    image: &str,
    needed_sha: Option<&str>,
    wait: Duration,
) -> HashMap<String, String> {
    let first = release_index(image, needed_sha);
    #[cfg(test)]
    {
        let _ = wait;
        first
    }
    #[cfg(not(test))]
    {
        let parsed = parse_image_ref(image);
        let Some(registry) = parsed.registry.as_deref() else {
            return first;
        };
        let key = release_cache_key(registry, &parsed.repository);
        let cached = cached_releases(&key);
        if let Some(entry) = cached.as_ref() {
            if !should_refresh_releases(entry, needed_sha) {
                return entry.by_sha.clone();
            }
        }
        wait_for_release_cache(&key, cached.as_ref().map(|entry| entry.at), wait)
            .await
            .unwrap_or(first)
    }
}

/// Which image's GitHub repo to ask for release tags.
///
/// A named running image (including a fork) owns its own tags. A bare
/// `sha256:…` id has no repo in the string — RepoDigests name it, and only
/// then does the panel's configured image stand in.
pub fn release_lookup_image(
    running_image: Option<&str>,
    repo_digests: &[String],
    configured: &str,
) -> String {
    if let Some(image) = running_image.filter(|s| !s.is_empty()) {
        if !is_content_digest_ref(image) && parse_image_ref(image).registry.is_some() {
            return image.to_string();
        }
    }
    if let Some(named) = named_repo_from_digests(repo_digests) {
        return named;
    }
    configured.to_string()
}

/// The commit a `sha-*` tag or OCI revision still names, when we have one.
///
/// Passed to [`release_index`] so a cache that predates the matching `v*` tag
/// is refreshed instead of pinning the SHA fallback for the full TTL.
pub fn release_sha_hint(image: Option<&str>, labels: &HashMap<String, String>) -> Option<String> {
    if let Some(image) = image {
        let parsed = parse_image_ref(image);
        if let Some(sha) = parsed.tag.strip_prefix("sha-") {
            return Some(sha.to_string());
        }
    }
    labels.get("org.opencontainers.image.revision").cloned()
}

fn named_repo_from_digests(digests: &[String]) -> Option<String> {
    digests.iter().find_map(|digest| {
        let (name, _) = digest.rsplit_once('@')?;
        parse_image_ref(name)
            .registry
            .is_some()
            .then(|| name.to_string())
    })
}

fn release_cache_key(registry: &str, repo: &str) -> String {
    format!("{registry}/{repo}")
}

#[cfg(not(test))]
fn cached_releases(key: &str) -> Option<ReleaseCacheEntry> {
    RELEASE_CACHE.lock().ok()?.get(key).cloned()
}

#[cfg(not(test))]
async fn wait_for_release_cache(
    key: &str,
    older_than: Option<Instant>,
    wait: Duration,
) -> Option<HashMap<String, String>> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(entry) = cached_releases(key) {
            if older_than.is_none_or(|at| entry.at > at) {
                return Some(entry.by_sha);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(not(test))]
fn should_refresh_releases(entry: &ReleaseCacheEntry, needed_sha: Option<&str>) -> bool {
    let needed_present = needed_sha
        .map(|sha| lookup_release(&entry.by_sha, sha).is_some())
        .unwrap_or(true);
    entry.at.elapsed() >= refresh_ttl(entry.fetched_ok, needed_present, entry.miss_streak)
}

fn refresh_ttl(fetched_ok: bool, needed_sha_present: bool, miss_streak: u32) -> Duration {
    if !fetched_ok {
        RETRY_TTL
    } else if needed_sha_present {
        CACHE_TTL
    } else {
        match miss_streak {
            0 | 1 => RETRY_TTL,
            2 => Duration::from_secs(60),
            3 => Duration::from_secs(120),
            _ => CACHE_TTL,
        }
    }
}

#[cfg(not(test))]
fn spawn_release_refresh(registry: String, repo: String, needed_sha: Option<String>) {
    let key = release_cache_key(&registry, &repo);
    let Some(gen) = begin_release_refresh(&key) else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        end_release_refresh(&key, gen);
        return;
    };
    handle.spawn(async move {
        let client = match reqwest::Client::builder()
            .user_agent(concat!("stitch-panel/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(_) => {
                end_release_refresh(&key, gen);
                return;
            }
        };
        let (by_sha, fetched_ok) = pull_release_tags(&client, &registry, &repo).await;
        let still_missing = needed_sha
            .as_deref()
            .is_some_and(|sha| lookup_release(&by_sha, sha).is_none());
        if let Ok(mut guard) = RELEASE_CACHE.lock() {
            let prev_miss = guard.get(&key).map(|e| e.miss_streak).unwrap_or(0);
            let miss_streak = if !fetched_ok {
                prev_miss
            } else if still_missing {
                prev_miss.saturating_add(1)
            } else {
                0
            };
            guard.insert(
                key.clone(),
                ReleaseCacheEntry {
                    at: Instant::now(),
                    repo,
                    by_sha,
                    fetched_ok,
                    miss_streak,
                },
            );
        }
        end_release_refresh(&key, gen);
    });
}

#[cfg(not(test))]
fn begin_release_refresh(key: &str) -> Option<u64> {
    let mut guard = RELEASE_INFLIGHT.lock().ok()?;
    if guard.contains_key(key) {
        return None;
    }
    let gen = RELEASE_REFRESH_GEN.fetch_add(1, Ordering::Relaxed);
    guard.insert(key.to_string(), gen);
    Some(gen)
}

#[cfg(not(test))]
fn end_release_refresh(key: &str, gen: u64) {
    if let Ok(mut guard) = RELEASE_INFLIGHT.lock() {
        if guard.get(key) == Some(&gen) {
            guard.remove(key);
        }
    }
}

/// The release tag for a running image, when one can be attributed.
///
/// Order: an image already tagged `vX.Y.Z`, then a `sha-*` tag looked up in
/// `releases`, then the OCI revision label (what `:latest` / a bare digest
/// still carries), then an OCI version label that is itself a release tag.
pub fn version_for_image(
    releases: &HashMap<String, String>,
    image: Option<&str>,
    labels: &HashMap<String, String>,
) -> Option<String> {
    if let Some(image) = image {
        let parsed = parse_image_ref(image);
        if is_release_tag(&parsed.tag) {
            return Some(display_release(&parsed.tag));
        }
        if let Some(sha) = parsed.tag.strip_prefix("sha-") {
            if let Some(version) = lookup_release(releases, sha) {
                return Some(version);
            }
        }
    }
    if let Some(revision) = labels.get("org.opencontainers.image.revision") {
        if let Some(version) = lookup_release(releases, revision) {
            return Some(version);
        }
    }
    labels
        .get("org.opencontainers.image.version")
        .filter(|value| is_release_tag(value))
        .map(|value| display_release(value))
}

/// True when `tag` is a cargo-dist / GitHub release (`v0.1.226` or `0.1.226`).
///
/// Refuses `latest`, branch names, and `sha-*` — those are channels or commits,
/// not a version number.
pub fn is_release_tag(tag: &str) -> bool {
    let rest = tag.strip_prefix('v').unwrap_or(tag);
    let mut parts = rest.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(major), Some(minor), Some(patch), None) => {
            is_digits(major) && is_digits(minor) && is_digits(patch)
        }
        _ => false,
    }
}

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
}

fn display_release(tag: &str) -> String {
    if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    }
}

fn lookup_release(releases: &HashMap<String, String>, sha: &str) -> Option<String> {
    let hex = sha.strip_prefix("sha-").unwrap_or(sha);
    if hex.len() < 7 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    releases.get(&hex[..hex.len().min(40)]).cloned()
}

async fn published_release_index(
    client: &reqwest::Client,
    registry: &str,
    repo: &str,
) -> HashMap<String, String> {
    #[cfg(test)]
    {
        let _ = (client, registry);
        TEST_RELEASES
            .with(|cell| cell.borrow().for_repo(repo))
            .unwrap_or_default()
    }
    #[cfg(not(test))]
    {
        pull_release_tags(client, registry, repo).await.0
    }
}

#[cfg_attr(test, allow(dead_code))]
async fn pull_release_tags(
    client: &reqwest::Client,
    registry: &str,
    repo: &str,
) -> (HashMap<String, String>, bool) {
    #[derive(serde::Deserialize)]
    struct Tag {
        name: String,
        commit: TagCommit,
    }
    #[derive(serde::Deserialize)]
    struct TagCommit {
        sha: String,
    }

    if registry != "ghcr.io" || repo.matches('/').count() != 1 {
        return (HashMap::new(), true);
    }

    let mut by_sha = HashMap::new();
    let mut fetched_ok = false;
    for page in 1..=5 {
        let url = format!("https://api.github.com/repos/{repo}/tags?per_page=100&page={page}");
        let Ok(res) = client.get(&url).send().await else {
            break;
        };
        let Ok(res) = res.error_for_status() else {
            break;
        };
        let Ok(tags) = res.json::<Vec<Tag>>().await else {
            break;
        };
        let count = tags.len();
        merge_release_tags(
            &mut by_sha,
            tags.into_iter()
                .filter(|tag| is_release_tag(&tag.name))
                .map(|tag| (tag.commit.sha, display_release(&tag.name))),
        );
        if count < 100 {
            fetched_ok = true;
            break;
        }
        if page == 5 {
            // Hit the page cap with a full last page — we have a usable prefix,
            // but it isn't the complete tag list.
            fetched_ok = true;
        }
    }

    (by_sha, fetched_ok)
}

fn merge_release_tags(
    into: &mut HashMap<String, String>,
    tags: impl IntoIterator<Item = (String, String)>,
) {
    for (sha, version) in tags {
        if sha.len() < 7 {
            continue;
        }
        for len in 7..=sha.len() {
            into.entry(sha[..len].to_string())
                .or_insert_with(|| version.clone());
        }
    }
}

#[cfg(test)]
pub fn set_test_release_index(index: HashMap<String, String>) {
    TEST_RELEASES.with(|cell| cell.borrow_mut().any = Some(index));
}

#[cfg(test)]
pub fn set_test_release_index_for_repo(repo: &str, index: HashMap<String, String>) {
    TEST_RELEASES.with(|cell| {
        cell.borrow_mut().by_repo.insert(repo.to_string(), index);
    });
}

#[cfg(test)]
pub fn clear_test_release_index() {
    TEST_RELEASES.with(|cell| *cell.borrow_mut() = TestReleaseIndex::default());
}

/// Index commits by every short-sha length a tag might use.
///
/// `docker/metadata-action` publishes a 7-char prefix by default but the length
/// is configurable, so the map holds each prefix from 7 chars up rather than
/// assuming one. Earlier commits win a collision — the list is newest first, and
/// a 7-hex-char collision in one repository is not a real case anyway.
fn index_commits(
    commits: impl IntoIterator<Item = (String, String, String)>,
) -> HashMap<String, CommitInfo> {
    let mut out = HashMap::new();
    for (position, (sha, date, subject)) in commits.into_iter().enumerate() {
        for len in 7..=sha.len() {
            out.entry(sha[..len].to_string())
                .or_insert_with(|| CommitInfo {
                    position,
                    date: date.clone(),
                    subject: subject.clone(),
                });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(tag: &str, digest: Option<&str>) -> PublishedVersion {
        PublishedVersion {
            tag: tag.into(),
            image: format!("ghcr.io/textile-protocol/textile-stitch:{tag}"),
            digest: digest.map(str::to_string),
            published_at: None,
            subject: None,
            version: None,
        }
    }

    #[test]
    fn only_tags_that_name_one_build_are_offered() {
        assert!(is_immutable_tag("sha-14cd877"));
        assert!(is_immutable_tag("sha-14cd8771"));
        // Channels move under the bot's feet, so pinning to one is not a pin.
        assert!(!is_immutable_tag("latest"));
        assert!(!is_immutable_tag("main"));
        assert!(!is_immutable_tag("sha-nothex"));
        assert!(!is_immutable_tag("sha-123"));
        // Immutable, but it names a release rather than a commit, so nothing
        // here can place it in the order. Offering it would mean mis-sorting it.
        assert!(!is_immutable_tag("v0.2.0"));
        assert!(!is_immutable_tag("0.2.0"));
    }

    #[test]
    fn a_channel_tag_is_refused_with_the_reason() {
        assert!(check_rollback_tag("sha-14cd877").is_ok());
        let err = check_rollback_tag("latest").unwrap_err();
        assert!(err.contains("Use Update"), "{err}");
        assert!(check_rollback_tag("  ").unwrap_err().contains("no version"));
    }

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Commits newest first, the order GitHub's endpoint answers in.
    fn commits(shas: &[&str]) -> HashMap<String, CommitInfo> {
        index_commits(shas.iter().map(|sha| {
            (
                format!("{sha}0000000000000000000000000000000"),
                "2026-08-10T12:00:00Z".to_string(),
                format!("feat: {sha}"),
            )
        }))
    }

    #[test]
    fn the_newest_tags_come_from_the_commit_order_not_the_tag_order() {
        // The registry hands them back lexically — which for `sha-<hex>` tags is
        // in effect random. Taking the tail would pick ccccccc/ddddddd and call
        // the oldest build "newest".
        let published = tags(&[
            "latest",
            "main",
            "sha-aaaaaaa",
            "sha-bbbbbbb",
            "sha-ccccccc",
            "sha-ddddddd",
        ]);
        let history = commits(&["bbbbbbb", "ddddddd", "aaaaaaa", "ccccccc"]);
        assert_eq!(
            recent_tags(&published, &history, 2),
            vec!["sha-bbbbbbb", "sha-ddddddd"]
        );
        // Fewer published than asked for is normal for a young repository.
        assert_eq!(recent_tags(&published, &history, 10).len(), 4);
        assert!(recent_tags(&[], &history, 10).is_empty());
    }

    #[test]
    fn tags_github_cannot_place_fill_the_leftover_slots() {
        // A build older than the commit window, or one from a branch that isn't
        // the default, must not displace a known release — but it's still a
        // published build, so it takes a slot no release wants.
        let published = tags(&["sha-0000000", "sha-aaaaaaa", "sha-9999999"]);
        let history = commits(&["aaaaaaa"]);
        assert_eq!(
            recent_tags(&published, &history, 3),
            // Known release first, then the registry's tail order for the rest.
            vec!["sha-aaaaaaa", "sha-9999999", "sha-0000000"]
        );
        assert_eq!(recent_tags(&published, &history, 1), vec!["sha-aaaaaaa"]);
    }

    #[test]
    fn a_row_nothing_could_place_sinks_the_newest_first_claim() {
        let placed = |tag: &str| PublishedVersion {
            published_at: Some("2026-08-10T12:00:00Z".into()),
            ..version(tag, None)
        };
        let bare = version("sha-bbbbbbb", None);
        assert_eq!(
            ordering_of(&[placed("sha-aaaaaaa"), placed("sha-ccccccc")]),
            VersionOrdering::Commit
        );
        // One unplaced row is enough: it sorts last but could be the newest
        // build of the lot, so the rows above it can't be called ranked.
        assert_eq!(
            ordering_of(&[placed("sha-aaaaaaa"), bare.clone()]),
            VersionOrdering::Partial
        );
        assert_eq!(ordering_of(&[bare]), VersionOrdering::Registry);
        // Nothing to claim about an empty list either.
        assert_eq!(ordering_of(&[]), VersionOrdering::Registry);
    }

    #[test]
    fn no_commit_data_falls_back_to_the_registrys_own_order() {
        // Private repo, rate limit, or a registry that isn't GHCR: the tail is
        // the newest wherever the registry appends, which is the best guess left.
        let published = tags(&["latest", "sha-aaaaaaa", "sha-bbbbbbb", "sha-ccccccc"]);
        assert_eq!(
            recent_tags(&published, &HashMap::new(), 2),
            vec!["sha-ccccccc", "sha-bbbbbbb"]
        );
    }

    #[test]
    fn the_rollback_target_keeps_the_configured_repository() {
        assert_eq!(
            rollback_image(
                "ghcr.io/textile-protocol/textile-stitch:latest",
                "sha-14cd877"
            )
            .as_deref(),
            Some("ghcr.io/textile-protocol/textile-stitch:sha-14cd877")
        );
        // A local-only image can't be pulled, so there is nothing to roll back to.
        assert!(rollback_image("stitch:latest", "sha-14cd877").is_none());
    }

    #[test]
    fn the_running_build_is_matched_by_tag_or_by_digest() {
        let pinned = version("sha-14cd877", Some("sha256:aaaa"));
        // Pinned by an earlier rollback: the ref says it outright.
        assert!(is_current(
            &pinned,
            &[],
            Some("ghcr.io/textile-protocol/textile-stitch:sha-14cd877")
        ));
        // On :latest — only the digest can say which build that resolved to.
        let local = vec!["ghcr.io/textile-protocol/textile-stitch@sha256:aaaa".to_string()];
        assert!(is_current(
            &pinned,
            &local,
            Some("ghcr.io/textile-protocol/textile-stitch:latest")
        ));
        assert!(!is_current(
            &version("sha-0000000", Some("sha256:bbbb")),
            &local,
            None
        ));
        // No digest and no tag match: unknown, which must not read as current.
        assert!(!is_current(&version("sha-0000000", None), &local, None));
    }

    #[test]
    fn pagination_follows_only_a_relative_next_link() {
        assert_eq!(
            next_page_url("</v2/org/name/tags/list?last=sha-aaa&n=1000>; rel=\"next\""),
            Some("/v2/org/name/tags/list?last=sha-aaa&n=1000")
        );
        assert_eq!(
            next_page_url(
                "</v2/a/tags/list?n=1>; rel=\"prev\", </v2/b/tags/list?n=2>; rel=\"next\""
            ),
            Some("/v2/b/tags/list?n=2")
        );
        assert_eq!(next_page_url("</v2/a/tags/list>; rel=\"prev\""), None);
        // An absolute URL would let a registry point the panel at another host.
        assert_eq!(
            next_page_url("<https://evil.example/v2/x>; rel=\"next\""),
            None
        );
    }

    #[test]
    fn commits_are_indexed_by_every_prefix_a_tag_could_use() {
        let index = index_commits([
            (
                "14cd87719df119b1933e599c74a0eeec65f20030".to_string(),
                "2026-08-10T12:57:50Z".to_string(),
                "feat: add custom corridor option".to_string(),
            ),
            (
                "94e838e19df119b1933e599c74a0eeec65f20030".to_string(),
                "2026-08-10T11:24:12Z".to_string(),
                "feat: take NVDA/USDG preset live".to_string(),
            ),
        ]);
        assert_eq!(index["14cd877"].subject, "feat: add custom corridor option");
        assert_eq!(index["14cd8771"].date, "2026-08-10T12:57:50Z");
        assert!(!index.contains_key("14cd87"));
        // Position is what ordering reads, and the endpoint answers newest first.
        assert_eq!(index["14cd877"].position, 0);
        assert_eq!(index["94e838e"].position, 1);
    }

    #[test]
    fn release_tags_are_the_semver_operators_read() {
        assert!(is_release_tag("v0.1.226"));
        assert!(is_release_tag("0.1.226"));
        assert!(!is_release_tag("latest"));
        assert!(!is_release_tag("sha-14cd877"));
        assert!(!is_release_tag("main"));
        assert_eq!(display_release("0.1.226"), "v0.1.226");
        assert_eq!(display_release("v0.1.226"), "v0.1.226");
    }

    #[test]
    fn a_running_image_resolves_to_its_release() {
        let mut releases = HashMap::new();
        merge_release_tags(
            &mut releases,
            [(
                "24e9192cce6d0000000000000000000000000000".into(),
                "v0.1.226".into(),
            )],
        );
        assert_eq!(
            version_for_image(
                &releases,
                Some("ghcr.io/textile-protocol/textile-stitch:sha-24e9192"),
                &HashMap::new(),
            )
            .as_deref(),
            Some("v0.1.226")
        );
        // :latest / a bare digest: only the OCI revision still names the commit.
        let labels = HashMap::from([(
            "org.opencontainers.image.revision".into(),
            "24e9192cce6d0000000000000000000000000000".into(),
        )]);
        assert_eq!(
            version_for_image(
                &releases,
                Some("sha256:2f7391aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4ab8b"),
                &labels,
            )
            .as_deref(),
            Some("v0.1.226")
        );
        assert_eq!(
            version_for_image(
                &HashMap::new(),
                Some("ghcr.io/textile-protocol/textile-stitch:v0.1.200"),
                &HashMap::new(),
            )
            .as_deref(),
            Some("v0.1.200")
        );
        // Channel tags and unknown digests stay unresolved — the UI keeps the image ref.
        assert_eq!(
            version_for_image(
                &releases,
                Some("ghcr.io/textile-protocol/textile-stitch:latest"),
                &HashMap::new(),
            ),
            None
        );
    }

    #[test]
    fn release_lookup_uses_the_running_image_repo_not_the_panel_default() {
        let configured = "ghcr.io/textile-protocol/textile-stitch:latest";
        assert_eq!(
            release_lookup_image(Some("ghcr.io/someone/fork:sha-24e9192"), &[], configured,),
            "ghcr.io/someone/fork:sha-24e9192"
        );
        // Bare id: RepoDigests name the repo. The configured image is last resort.
        assert_eq!(
            release_lookup_image(
                Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                &["ghcr.io/someone/fork@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
                configured,
            ),
            "ghcr.io/someone/fork"
        );
        assert_eq!(
            release_lookup_image(
                Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                &[],
                configured,
            ),
            configured
        );
    }

    #[test]
    fn release_cache_keys_are_per_registry_and_repository() {
        assert_ne!(
            release_cache_key("ghcr.io", "textile-protocol/textile-stitch"),
            release_cache_key("ghcr.io", "someone/fork")
        );
        assert_eq!(
            release_cache_key("ghcr.io", "someone/fork"),
            "ghcr.io/someone/fork"
        );
    }

    #[test]
    fn a_missing_release_or_failed_fetch_retries_sooner_than_a_hit() {
        assert_eq!(refresh_ttl(true, true, 0), CACHE_TTL);
        assert_eq!(refresh_ttl(true, false, 1), RETRY_TTL);
        assert_eq!(refresh_ttl(true, false, 4), CACHE_TTL);
        assert_eq!(refresh_ttl(false, true, 0), RETRY_TTL);
        assert_eq!(
            release_sha_hint(
                Some("ghcr.io/textile-protocol/textile-stitch:sha-24e9192"),
                &HashMap::new(),
            )
            .as_deref(),
            Some("24e9192")
        );
    }

    #[test]
    fn a_commit_body_never_reaches_the_picker() {
        // Conventional commits carry a body the width of a paragraph; the row
        // has space for the subject.
        assert_eq!(
            subject_line("fix: one thing\n\nWhy it broke, at length.\n"),
            "fix: one thing"
        );
        assert_eq!(subject_line(""), "");
    }
}

//! GitHub Releases source — the daemon channel.
//!
//! "Latest" is the highest **semver** among tags matching `tag_prefix` that has a published
//! release, not `/releases/latest`: that endpoint is repo-wide, so it breaks the moment a
//! second channel shares the repo, and it answers "most recently published" rather than
//! "highest version" — which differ as soon as you publish a patch to an older line.
//!
//! The candidates come from the channel's **tags**, not from listing releases. The release
//! listing carries every dev build and every asset of each, so its size grows with the whole
//! repository's activity and nothing on the robot controls it — at 67 releases its first page
//! passed the 1 MiB metadata cap and every robot stopped updating (#421). The tag list is
//! filtered by prefix on GitHub's side and costs ~440 bytes a release of *this* channel.
//! See `docs/design/updater-design.md` §6.
//!
//! Nothing here is trusted. Tags, release names and asset URLs are all attacker- or
//! mistake-influenced; the only thing that makes an artifact acceptable is the
//! minisign signature the caller checks afterwards.

use std::path::Path;

use serde::Deserialize;

use crate::Error;
use crate::manifest::Manifest;
use crate::source::{FetchedArtifact, ProgressSink, SignedBytes, Source, http};

/// Where the GitHub API lives. A field rather than a literal only so tests can serve it.
const API_BASE: &str = "https://api.github.com";

/// Tags tried, newest first, before giving up on finding one with a published release.
///
/// A tag without a usable release is normal but rare: a release still building, a tag pushed by
/// hand (`daemon-v0.5.0` has none), a draft. Each one skipped costs a request against an
/// unauthenticated limit of 60 an hour, so a run of them is a fault to report, not to walk.
const MAX_CANDIDATES: usize = 5;

/// The signature that accompanies every signed file.
const SIG_SUFFIX: &str = ".minisig";

/// What the release-asset API needs to return bytes rather than JSON metadata.
const OCTET_STREAM: &str = "application/octet-stream";

pub struct GithubReleases {
    api_base: String,
    repo: String,
    tag_prefix: String,
    manifest_asset: String,
    ref_tag_prefix: String,
    staging_tag_prefix: String,
    client: reqwest::Client,
}

/// Only the fields we use. GitHub adds fields freely, so this is deliberately not
/// exhaustive.
#[derive(Debug, Deserialize)]
struct Release {
    /// What [`GithubReleases::release_for_tag`] re-asks by when the tag answer lists nothing.
    id: u64,
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// One entry of `git/matching-refs/tags/<prefix>`.
#[derive(Debug, Deserialize)]
struct TagRef {
    /// `refs/tags/<tag>`.
    #[serde(rename = "ref")]
    git_ref: String,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    /// The API endpoint for this asset, `/repos/{owner}/{repo}/releases/assets/{id}`.
    ///
    /// Used in preference to `browser_download_url` because that one **404s on a private
    /// repository**, with or without a token — verified against this repo. The API endpoint
    /// serves the bytes with a token and `Accept: application/octet-stream`, and works for
    /// public repos too, so there is one path rather than two.
    url: String,
}

impl GithubReleases {
    pub fn new(
        repo: String,
        tag_prefix: String,
        manifest_asset: String,
        ref_tag_prefix: String,
        staging_tag_prefix: String,
    ) -> Self {
        Self {
            api_base: API_BASE.to_owned(),
            repo,
            tag_prefix,
            ref_tag_prefix,
            staging_tag_prefix,
            manifest_asset,
            // A failure here means a broken TLS setup, which is fatal for every
            // request anyway; fall back to a default client so construction stays
            // infallible and the error surfaces on first use.
            client: http::client().unwrap_or_default(),
        }
    }

    fn tag_for(&self, version: &semver::Version) -> String {
        format!("{}{}", self.tag_prefix, version)
    }

    /// The tag a named ref resolves to: `daemon-dev-` + the branch name.
    ///
    /// The ref is appended verbatim. Branch names are already valid git refs, so slashes in
    /// `feature/foo` need no handling — and rewriting them would resolve to a tag that does
    /// not exist, failing with "release not found" instead of anything informative.
    fn ref_tag_for(&self, git_ref: &str) -> String {
        format!("{}{}", self.ref_tag_prefix, git_ref)
    }

    /// The tag a release candidate lives under: `daemon-staging-v` + the version.
    fn staging_tag_for(&self, version: &semver::Version) -> String {
        format!("{}{}", self.staging_tag_prefix, version)
    }

    /// The release behind `tag`, with its assets.
    ///
    /// **An empty asset list from the tag lookup is asked again by id.** From 2026-09-23 the
    /// unauthenticated `releases/tags/{tag}` answer — and the listing — carry no assets for any
    /// release published since, while `releases/{id}` for the same release lists every one. A
    /// board asks without a token, so it saw a finished build as an empty one and refused it,
    /// every time. The second request costs nothing on a release that really is empty, and it
    /// is what makes [`Error::ReleaseNotReady`] mean what it says.
    async fn release_for_tag(&self, tag: &str) -> Result<Release, Error> {
        self.release_if_any(tag).await?.ok_or_else(|| {
            Error::Network(format!(
                "no release for tag {tag} in {} (wrong tag? or a private repo — set GITHUB_TOKEN?)",
                self.repo
            ))
        })
    }

    /// [`Self::release_for_tag`], with "this tag has no release" as `None`.
    async fn release_if_any(&self, tag: &str) -> Result<Option<Release>, Error> {
        let url = format!("{}/repos/{}/releases/tags/{tag}", self.api_base, self.repo);
        let Some(release) = self.release_at(&url, tag).await? else {
            return Ok(None);
        };
        if !release.assets.is_empty() {
            return Ok(Some(release));
        }

        let url = format!(
            "{}/repos/{}/releases/{}",
            self.api_base, self.repo, release.id
        );
        tracing::debug!(%tag, id = release.id, "tag lookup listed no assets; asking by id");
        self.release_at(&url, tag).await
    }

    async fn release_at(&self, url: &str, tag: &str) -> Result<Option<Release>, Error> {
        let Some(bytes) =
            http::get_bytes_if_found(&self.client, url, Some("application/vnd.github+json"))
                .await?
        else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Network(format!("parsing release {tag}: {e}")))
    }

    /// Highest **stable** semver among matching tags.
    ///
    /// Drafts and prereleases are skipped: a draft isn't published, and a prerelease is
    /// by definition not what a client robot should install. Reach one with an explicit
    /// `--version` or `--ref`.
    ///
    /// Two independent reasons a build is skipped — GitHub's `prerelease` flag *and* a
    /// semver prerelease component — because dev builds (`0.2.0-dev.5.abc1234`) must
    /// never become `latest` for the fleet, and relying on someone remembering a
    /// checkbox is not a safeguard.
    async fn newest_release(&self) -> Result<Release, Error> {
        self.newest_under(&self.tag_prefix, false).await
    }

    /// The newest release candidate, which is the same scan with the prerelease flag allowed.
    ///
    /// Only the *GitHub* flag is allowed, never a semver prerelease: candidates carry the plain
    /// version they will be promoted under (`0.3.0`), while dev builds carry `0.3.0-dev.17.abc`
    /// and live under a third prefix. So the two exclusions in [`Self::newest_under`] are not
    /// redundant here — dropping one still excludes branch builds, which is the point.
    async fn newest_staging_release(&self) -> Result<Release, Error> {
        self.newest_under(&self.staging_tag_prefix, true).await
    }

    /// The release of the highest version among tags carrying `prefix`.
    ///
    /// Versions come from the tags, newest first; the first whose release exists, is not a
    /// draft and carries the right prerelease flag is the answer. That is the same choice the
    /// release listing gave — a tag with no release never appeared in it — made without
    /// fetching anyone else's releases.
    ///
    /// `allow_flagged_prerelease` is the whole difference between the stable channel and the
    /// staging one, and it is a parameter rather than a field so that the call site — one of
    /// exactly two — states which channel it means.
    async fn newest_under(
        &self,
        prefix: &str,
        allow_flagged_prerelease: bool,
    ) -> Result<Release, Error> {
        // Not paginated: GitHub returns every match in one answer.
        let url = format!(
            "{}/repos/{}/git/matching-refs/tags/{prefix}",
            self.api_base, self.repo
        );
        let bytes =
            http::get_bytes(&self.client, &url, Some("application/vnd.github+json")).await?;
        let refs: Vec<TagRef> = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Network(format!("parsing tags: {e}")))?;

        let candidates = newest_first(prefix, refs.iter().map(|r| r.git_ref.as_str()));
        if candidates.is_empty() {
            return Err(Error::Network(format!(
                "no releases in {} with tag prefix {prefix:?}",
                self.repo
            )));
        }

        let mut skipped = Vec::new();
        for version in candidates.into_iter().take(MAX_CANDIDATES) {
            let tag = format!("{prefix}{version}");
            match self.release_if_any(&tag).await? {
                Some(release)
                    if !release.draft && (allow_flagged_prerelease || !release.prerelease) =>
                {
                    return Ok(release);
                }
                Some(_) => skipped.push(format!("{tag} (draft or prerelease)")),
                None => skipped.push(format!("{tag} (no release)")),
            }
            tracing::debug!(%tag, "newest tag has no usable release; trying the one below");
        }

        Err(Error::Network(format!(
            "no usable release among the newest {prefix:?} tags in {}: {}",
            self.repo,
            skipped.join(", ")
        )))
    }

    /// The API download URL for a named asset. Pair it with [`OCTET_STREAM`].
    ///
    /// A release with *no* assets at all is reported separately, because it is not the same
    /// situation as a missing one. Assets are uploaded at the end of a release build, so an
    /// empty release is every release for the few minutes before that finishes — a state to
    /// wait out, not a fault to investigate. A release that has assets but not this one is the
    /// fault case, and there the list of what *is* there is the whole diagnostic.
    fn asset_url(&self, release: &Release, name: &str) -> Result<String, Error> {
        if release.assets.is_empty() {
            return Err(Error::ReleaseNotReady {
                repo: self.repo.clone(),
                tag: release.tag_name.clone(),
            });
        }

        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.url.clone())
            .ok_or_else(|| {
                let available: Vec<_> = release.assets.iter().map(|a| a.name.as_str()).collect();
                Error::Network(format!(
                    "release {} has no asset named {name:?} (has: {})",
                    release.tag_name,
                    available.join(", ")
                ))
            })
    }

    /// Split one of our own release-download URLs into `(tag, asset name)`.
    ///
    /// `None` for anything else, including another repository's release URL — which matters
    /// because the manifest this comes from is unverified at that point, so a URL naming a
    /// foreign repo must not become an authenticated API request against it.
    fn split_release_url(&self, url: &str) -> Option<(String, String)> {
        let prefix = format!("https://github.com/{}/releases/download/", self.repo);
        let (tag, name) = url.strip_prefix(&prefix)?.split_once('/')?;
        Some((tag.to_owned(), name.to_owned()))
    }

    /// Where to actually fetch a URL from a signed manifest, and with which `Accept`.
    ///
    /// A private repo's `releases/download/...` URL 404s even with a token, so one of ours is
    /// re-resolved through the release API. Anything else is fetched verbatim — a manifest
    /// pointing at a CDN keeps working, and the bytes are hash- and signature-checked either
    /// way.
    async fn resolve_download(&self, url: &str) -> Result<(String, Option<&'static str>), Error> {
        let Some((tag, name)) = self.split_release_url(url) else {
            return Ok((url.to_owned(), None));
        };

        let release = self.release_for_tag(&tag).await?;
        let api_url = self.asset_url(&release, &name)?;
        tracing::debug!(%tag, %name, "resolved asset through the release API");
        Ok((api_url, Some(OCTET_STREAM)))
    }

    async fn signed_manifest(&self, tag: &str) -> Result<SignedBytes<Manifest>, Error> {
        let release = self.release_for_tag(tag).await?;
        self.signed_manifest_of(&release).await
    }

    async fn signed_manifest_of(&self, release: &Release) -> Result<SignedBytes<Manifest>, Error> {
        let manifest_url = self.asset_url(release, &self.manifest_asset)?;
        let sig_name = format!("{}{SIG_SUFFIX}", self.manifest_asset);
        let sig_url = self.asset_url(release, &sig_name)?;

        let bytes = http::get_bytes(&self.client, &manifest_url, Some(OCTET_STREAM)).await?;
        let signature = http::get_bytes(&self.client, &sig_url, Some(OCTET_STREAM)).await?;

        // Parsed for convenience only; the *bytes* are what the caller verifies,
        // since the signature covers exactly what was received.
        let parsed: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Corrupt(format!("manifest at {manifest_url}: {e}")))?;

        Ok(SignedBytes {
            bytes,
            signature,
            parsed,
        })
    }
}

#[async_trait::async_trait]
impl Source for GithubReleases {
    async fn latest_manifest(&self) -> Result<SignedBytes<Manifest>, Error> {
        let release = self.newest_release().await?;
        tracing::debug!(repo = %self.repo, tag = %release.tag_name, "resolved latest");
        self.signed_manifest_of(&release).await
    }

    async fn manifest_for(
        &self,
        version: &semver::Version,
    ) -> Result<SignedBytes<Manifest>, Error> {
        self.signed_manifest(&self.tag_for(version)).await
    }

    async fn manifest_at_ref(&self, git_ref: &str) -> Result<SignedBytes<Manifest>, Error> {
        let tag = self.ref_tag_for(git_ref);
        tracing::debug!(repo = %self.repo, %tag, %git_ref, "resolving ref");
        self.signed_manifest(&tag).await
    }

    async fn staging_manifest(&self) -> Result<SignedBytes<Manifest>, Error> {
        let release = self.newest_staging_release().await?;
        tracing::debug!(repo = %self.repo, tag = %release.tag_name, "resolved newest candidate");
        self.signed_manifest_of(&release).await
    }

    async fn staging_manifest_for(
        &self,
        version: &semver::Version,
    ) -> Result<SignedBytes<Manifest>, Error> {
        self.signed_manifest(&self.staging_tag_for(version)).await
    }

    async fn fetch_artifact(
        &self,
        manifest: &Manifest,
        dest_dir: &Path,
        progress: ProgressSink,
    ) -> Result<FetchedArtifact, Error> {
        tokio::fs::create_dir_all(dest_dir)
            .await
            .map_err(|e| Error::Io {
                path: dest_dir.to_path_buf(),
                source: e,
            })?;

        // The filename comes from a signed manifest, but the signature is only
        // checked *after* download — so treat it as untrusted here and refuse
        // anything that isn't a bare name.
        let artifact_name = safe_file_name(&manifest.url)?;
        let artifact = dest_dir.join(&artifact_name);
        let signature = dest_dir.join(format!("{artifact_name}{SIG_SUFFIX}"));

        let (artifact_url, accept) = self.resolve_download(&manifest.url).await?;
        let bytes =
            http::download_to(&self.client, &artifact_url, &artifact, accept, &progress).await?;

        let (sig_url, sig_accept) = self.resolve_download(&manifest.sig_url).await?;
        let sig_bytes = http::get_bytes(&self.client, &sig_url, sig_accept).await?;
        tokio::fs::write(&signature, &sig_bytes)
            .await
            .map_err(|e| Error::Io {
                path: signature.clone(),
                source: e,
            })?;

        Ok(FetchedArtifact {
            artifact,
            signature,
            bytes,
        })
    }
}

/// Parse a version out of a tag carrying `prefix`, or `None` if the tag is not one of ours.
///
/// Free-standing because two prefixes now feed it — stable and staging — and a method reading
/// `self.tag_prefix` invited exactly the bug where a staging scan silently measured stable tags.
fn version_under(prefix: &str, tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.strip_prefix(prefix)?).ok()
}

/// The release versions among `refs/tags/...` names, highest first.
///
/// A semver prerelease is a dev build, whatever its tag prefix, and is never a candidate for
/// either channel.
fn newest_first<'a>(prefix: &str, refs: impl Iterator<Item = &'a str>) -> Vec<semver::Version> {
    let mut versions: Vec<_> = refs
        .filter_map(|r| r.strip_prefix("refs/tags/"))
        .filter_map(|tag| version_under(prefix, tag))
        .filter(|v| v.pre.is_empty())
        .collect();
    versions.sort_unstable_by(|a, b| b.cmp(a));
    versions.dedup();
    versions
}

/// Extract a filename from a URL, refusing anything that could escape `dest_dir`.
///
/// A manifest is signed, but this runs *before* verification, and a compromised
/// publisher is exactly the case where writing to an arbitrary path would matter.
pub(crate) fn safe_file_name(url: &str) -> Result<String, Error> {
    let tail = url.rsplit('/').next().unwrap_or_default();
    let tail = tail.split(['?', '#']).next().unwrap_or_default();

    let looks_safe = !tail.is_empty()
        && tail != "."
        && tail != ".."
        && !tail.contains('/')
        && !tail.contains('\\')
        && !tail.contains('\0');

    if looks_safe {
        Ok(tail.to_owned())
    } else {
        Err(Error::Verification(format!(
            "manifest url {url:?} does not end in a usable filename"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> GithubReleases {
        GithubReleases::new(
            "ORG/robot-daemon".into(),
            "daemon-v".into(),
            "manifest.json".into(),
            "daemon-dev-".into(),
            "daemon-staging-v".into(),
        )
    }

    /// The candidates are this channel's release versions, highest first — by semver, not by
    /// the order GitHub lists refs in, which is lexical (`0.10.0` before `0.9.3`).
    #[test]
    fn candidates_are_release_versions_highest_first() {
        let refs = [
            "refs/tags/daemon-v0.10.0",
            "refs/tags/daemon-v0.17.4",
            "refs/tags/daemon-v0.9.3",
            "refs/tags/daemon-v0.18.0-dev.3.abc1234",
            "refs/tags/daemon-vnot-a-version",
        ];
        let got: Vec<String> = newest_first("daemon-v", refs.into_iter())
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(got, ["0.17.4", "0.10.0", "0.9.3"]);
    }

    /// A fake GitHub serving `tags` as the channel's tag refs and `releases` by tag.
    async fn fake_github(
        tags: &'static [&'static str],
        releases: Vec<(&'static str, serde_json::Value)>,
    ) -> GithubReleases {
        use axum::extract::Path;
        use axum::http::StatusCode;
        use axum::routing::get;

        let releases: std::collections::HashMap<_, _> = releases.into_iter().collect();
        let app = axum::Router::new()
            .route(
                "/repos/ORG/robot-daemon/git/matching-refs/tags/{prefix}",
                get(move |Path(prefix): Path<String>| async move {
                    let refs: Vec<_> = tags
                        .iter()
                        .filter(|t| t.starts_with(&prefix))
                        .map(|t| serde_json::json!({ "ref": format!("refs/tags/{t}") }))
                        .collect();
                    axum::Json(refs)
                }),
            )
            .route(
                "/repos/ORG/robot-daemon/releases/tags/{tag}",
                get(move |Path(tag): Path<String>| async move {
                    match releases.get(tag.as_str()) {
                        Some(r) => Ok(axum::Json(r.clone())),
                        None => Err(StatusCode::NOT_FOUND),
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let mut s = source();
        s.api_base = format!("http://{addr}");
        s
    }

    fn release(tag: &str, prerelease: bool, draft: bool) -> serde_json::Value {
        serde_json::json!({
            "id": 1, "tag_name": tag, "prerelease": prerelease, "draft": draft,
            "assets": [{ "name": "manifest.json", "url": "https://example/a/1" }],
        })
    }

    /// The newest tag wins when it has a release — even listed after older ones.
    #[tokio::test]
    async fn latest_is_the_highest_tag_with_a_release() {
        let gh = fake_github(
            &[
                "daemon-v0.10.0",
                "daemon-v0.17.4",
                "daemon-v0.9.3",
                "daemon-dev-main",
            ],
            vec![
                ("daemon-v0.10.0", release("daemon-v0.10.0", false, false)),
                ("daemon-v0.17.4", release("daemon-v0.17.4", false, false)),
                ("daemon-v0.9.3", release("daemon-v0.9.3", false, false)),
            ],
        )
        .await;
        assert_eq!(
            gh.newest_release().await.unwrap().tag_name,
            "daemon-v0.17.4"
        );
    }

    /// **A tag without a usable release is passed over, not fatal.** A release is built after
    /// its tag is pushed, so for its whole build the newest tag has none, and a hand-pushed tag
    /// never gets one. The fleet must keep seeing the release below rather than fail its check.
    #[tokio::test]
    async fn tags_without_a_usable_release_fall_through_to_the_next() {
        let gh = fake_github(
            &[
                "daemon-v0.18.0",
                "daemon-v0.17.5",
                "daemon-v0.17.6",
                "daemon-v0.17.4",
            ],
            vec![
                // 0.18.0 is still building: tag, no release.
                ("daemon-v0.17.6", release("daemon-v0.17.6", false, true)),
                ("daemon-v0.17.5", release("daemon-v0.17.5", true, false)),
                ("daemon-v0.17.4", release("daemon-v0.17.4", false, false)),
            ],
        )
        .await;
        assert_eq!(
            gh.newest_release().await.unwrap().tag_name,
            "daemon-v0.17.4"
        );
    }

    /// Staging allows the prerelease flag; stable tags never leak into it.
    #[tokio::test]
    async fn staging_reads_only_staging_tags() {
        let gh = fake_github(
            &["daemon-v0.18.0", "daemon-staging-v0.17.5"],
            vec![
                ("daemon-v0.18.0", release("daemon-v0.18.0", false, false)),
                (
                    "daemon-staging-v0.17.5",
                    release("daemon-staging-v0.17.5", true, false),
                ),
            ],
        )
        .await;
        assert_eq!(
            gh.newest_staging_release().await.unwrap().tag_name,
            "daemon-staging-v0.17.5"
        );
    }

    /// A run of unusable tags is reported, naming each, instead of walked to the bottom.
    #[tokio::test]
    async fn a_run_of_releaseless_tags_is_reported() {
        let gh = fake_github(
            &[
                "daemon-v0.1.0",
                "daemon-v0.2.0",
                "daemon-v0.3.0",
                "daemon-v0.4.0",
                "daemon-v0.5.0",
                "daemon-v0.6.0",
            ],
            vec![("daemon-v0.1.0", release("daemon-v0.1.0", false, false))],
        )
        .await;
        let err = gh.newest_release().await.unwrap_err().to_string();
        assert!(err.contains("daemon-v0.6.0 (no release)"), "{err}");
        assert!(!err.contains("daemon-v0.1.0"), "{err}");
    }

    #[test]
    fn tags_round_trip() {
        let s = source();
        let v = semver::Version::new(1, 4, 2);
        assert_eq!(s.tag_for(&v), "daemon-v1.4.2");
        assert_eq!(version_under(&s.tag_prefix, "daemon-v1.4.2"), Some(v));
    }

    #[test]
    fn staging_tags_round_trip() {
        let s = source();
        let v = semver::Version::new(0, 3, 0);
        assert_eq!(s.staging_tag_for(&v), "daemon-staging-v0.3.0");
        assert_eq!(
            version_under("daemon-staging-v", "daemon-staging-v0.3.0"),
            Some(v)
        );
    }

    /// The two channels must not read each other's tags.
    ///
    /// This is the failure that would matter and would not look like one: a staging scan that
    /// silently matched `daemon-v*` would report the newest *stable* release as the candidate,
    /// and `--staging` would install what a plain `apply` already installs while claiming to
    /// test something. It holds because `daemon-staging-v0.3.0` does not start with `daemon-v`
    /// — an accident of naming, so it is pinned here rather than left to be re-derived.
    #[test]
    fn the_two_channels_cannot_read_each_others_tags() {
        assert_eq!(version_under("daemon-v", "daemon-staging-v0.3.0"), None);
        assert_eq!(version_under("daemon-staging-v", "daemon-v0.3.0"), None);
        // And neither reads a dev build, which is what keeps `--staging` from resolving to a
        // branch someone pushed.
        assert_eq!(
            version_under("daemon-staging-v", "daemon-dev-my-branch"),
            None
        );
    }

    /// A ref becomes a dev tag, and the ref is appended verbatim.
    ///
    /// The slash case is the one that matters: `feature/foo` is a valid branch name, so
    /// anything that sanitised it would resolve to a tag nobody published and fail with
    /// "release not found" rather than anything that points at the cause.
    #[test]
    fn refs_become_dev_tags_verbatim() {
        let s = source();
        assert_eq!(s.ref_tag_for("my-branch"), "daemon-dev-my-branch");
        assert_eq!(s.ref_tag_for("feature/foo"), "daemon-dev-feature/foo");
    }

    /// **A dev tag must never be mistaken for a release.** `version_under` drives
    /// `newest_version`, which is what the fleet installs — so if a dev tag parsed as a
    /// version here, a branch build could become `latest` for every robot. That is the
    /// failure the two independent guards exist to prevent, and this is the first of them.
    #[test]
    fn a_dev_tag_is_not_a_release_version() {
        let s = source();
        assert_eq!(version_under(&s.tag_prefix, "daemon-dev-my-branch"), None);
        // Even when the dev tag ends in something version-shaped.
        assert_eq!(version_under(&s.tag_prefix, "daemon-dev-0.2.0"), None);
        // And the staging stream stays separate too.
        assert_eq!(version_under(&s.tag_prefix, "daemon-staging-v0.2.0"), None);
    }

    /// Another channel's tags in the same repo must be ignored, not misparsed.
    #[test]
    fn foreign_tags_are_ignored() {
        let s = source();
        assert_eq!(version_under(&s.tag_prefix, "model-v3.0.0"), None);
        assert_eq!(version_under(&s.tag_prefix, "v1.0.0"), None);
        assert_eq!(version_under(&s.tag_prefix, "daemon-vnot-a-version"), None);
    }

    #[test]
    fn asset_lookup_lists_what_was_available_on_failure() {
        let release = Release {
            id: 1,
            tag_name: "daemon-v1.0.0".into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: "other.txt".into(),
                url: "https://api.github.com/repos/ORG/robot-daemon/releases/assets/1".into(),
            }],
        };
        let err = source().asset_url(&release, "manifest.json").unwrap_err();
        // A support ticket needs to see what *was* there.
        assert!(err.to_string().contains("other.txt"), "{err}");
    }

    /// A release whose build has not uploaded yet must not read as a broken release.
    ///
    /// This is what an operator hits by running `update apply --staging` in the minutes after
    /// publishing, and the old answer — `network error: ... has no asset named "manifest.json"
    /// (has: )` — pointed at the two things that were not wrong: the release, and the network.
    #[test]
    fn an_empty_release_says_its_build_has_not_finished() {
        let release = Release {
            id: 1,
            tag_name: "daemon-staging-v0.5.1".into(),
            draft: false,
            prerelease: true,
            assets: vec![],
        };

        let err = source().asset_url(&release, "manifest.json").unwrap_err();
        assert!(
            matches!(err, Error::ReleaseNotReady { .. }),
            "an empty release is a wait, not a fetch failure, got {err:?}"
        );

        let msg = err.to_string();
        // The tag, so it is clear *which* release, and where to watch it land.
        assert!(msg.contains("daemon-staging-v0.5.1"), "{msg}");
        assert!(
            msg.contains("https://github.com/ORG/robot-daemon/releases/tag/daemon-staging-v0.5.1"),
            "{msg}"
        );
        // And none of the vocabulary that sent people debugging the wrong thing.
        assert!(!msg.contains("network"), "{msg}");
        assert!(!msg.contains("no asset named"), "{msg}");
    }

    /// **A manifest must not redirect the asset lookup at another repository.**
    ///
    /// `resolve_download` runs before the manifest's signature is checked, so a URL naming a
    /// foreign repo must be left alone rather than turned into an API request carrying our
    /// token.
    #[test]
    fn only_our_own_release_urls_are_split() {
        let s = source();

        assert_eq!(
            s.split_release_url(
                "https://github.com/ORG/robot-daemon/releases/download/daemon-dev-my-branch/daemon-0.2.0-dev.1.abc1234.tar.zst"
            ),
            Some((
                "daemon-dev-my-branch".to_owned(),
                "daemon-0.2.0-dev.1.abc1234.tar.zst".to_owned()
            ))
        );

        for foreign in [
            "https://github.com/attacker/repo/releases/download/v1/x.tar.zst",
            "https://cdn.example.com/daemon-1.0.0.tar.zst",
            // Ours, but not a release-download URL.
            "https://github.com/ORG/robot-daemon/archive/refs/heads/main.tar.gz",
        ] {
            assert_eq!(s.split_release_url(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn accepts_a_plain_filename() {
        assert_eq!(
            safe_file_name("https://example.com/a/b/daemon-1.0.0.tar.zst").unwrap(),
            "daemon-1.0.0.tar.zst"
        );
        // Query strings are common on signed CDN URLs.
        assert_eq!(
            safe_file_name("https://example.com/x.tar.zst?token=abc").unwrap(),
            "x.tar.zst"
        );
    }

    /// The download path must not be steerable by a manifest.
    #[test]
    fn refuses_names_that_could_escape() {
        for url in [
            "https://example.com/",
            "https://example.com/..",
            "https://example.com/.",
            "https://example.com/a/",
        ] {
            assert!(safe_file_name(url).is_err(), "should refuse {url}");
        }
    }

    /// A dev build must never be selected as `latest`, even if whoever published it
    /// forgot to tick "prerelease". Fleet-wide auto-updates read `latest`.
    #[test]
    fn dev_versions_are_recognised_as_prereleases() {
        let s = source();
        let dev = version_under(&s.tag_prefix, "daemon-v0.2.0-dev.5.abc1234").unwrap();
        assert!(
            !dev.pre.is_empty(),
            "dev builds must carry a semver prerelease"
        );

        let stable = version_under(&s.tag_prefix, "daemon-v0.2.0").unwrap();
        assert!(stable.pre.is_empty());
        // And a dev build sorts *below* the release it precedes, so it can never look
        // like an upgrade from it.
        assert!(dev < stable);
    }

    /// GitHub adds response fields regularly; deserialisation must not break.
    #[test]
    fn unknown_release_fields_are_tolerated() {
        let json = serde_json::json!({
            "id": 7,
            "tag_name": "daemon-v1.0.0",
            "some_new_field": 42,
            "assets": [{
                "name": "manifest.json",
                "url": "https://api.github.com/repos/ORG/robot-daemon/releases/assets/7",
                "browser_download_url": "https://example/m.json",
                "another_new_field": true
            }]
        });
        let release: Release = serde_json::from_value(json).unwrap();
        assert_eq!(release.tag_name, "daemon-v1.0.0");
        assert!(!release.draft, "missing `draft` should default to false");
        // `url` is required, not defaulted: it is how assets are fetched, and a release whose
        // assets lack it is not something to paper over with an empty string that would fail
        // later as a confusing HTTP error.
        assert!(release.assets[0].url.contains("/releases/assets/"));
    }
}

//! Pure source, tag, and release-asset identity policy for self-update.

use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::Deserialize;
use url::{Host, Url};

pub(crate) const REPO: &str = "trailofbits/coop";
const OFFICIAL_API_ORIGIN: &str = "https://api.github.com";
const OFFICIAL_ASSET_ORIGIN: &str = "https://github.com";
pub(crate) const LEGACY_API_OVERRIDE: &str = "COOP_UPDATE_API_BASE_URL";
pub(crate) const TEST_API_BASE_URL: &str = "COOP_UPDATE_TEST_API_BASE_URL";
pub(crate) const TEST_VERIFY_ATTESTATION: &str = "COOP_UPDATE_TEST_VERIFY_ATTESTATION";
pub(crate) const BUNDLE_ASSET: &str = "attestations.jsonl";

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateSourceKind {
    Official,
    TestFixture {
        api_base: Url,
        verify_attestation: bool,
    },
}

/// Opaque capability describing the only update origins the current build may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateSource {
    kind: UpdateSourceKind,
}

impl UpdateSource {
    pub(crate) fn official() -> Self {
        Self {
            kind: UpdateSourceKind::Official,
        }
    }

    pub(crate) fn for_current_build(
        legacy_override_present: bool,
        fixture_base: Option<&str>,
        verify_attestation: bool,
    ) -> Result<Self> {
        Self::for_build(
            env!("COOP_BUILD_KIND"),
            legacy_override_present,
            fixture_base,
            verify_attestation,
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test_build_kind(
        build_kind: &str,
        legacy_override_present: bool,
        fixture_base: Option<&str>,
        verify_attestation: bool,
    ) -> Result<Self> {
        Self::for_build(
            build_kind,
            legacy_override_present,
            fixture_base,
            verify_attestation,
        )
    }

    fn for_build(
        build_kind: &str,
        legacy_override_present: bool,
        fixture_base: Option<&str>,
        verify_attestation: bool,
    ) -> Result<Self> {
        if build_kind != "test" {
            ensure!(
                !legacy_override_present,
                "update source policy rejected {LEGACY_API_OVERRIDE}: official builds use only \
                 {OFFICIAL_API_ORIGIN}/repos/{REPO}/releases"
            );
            return Ok(Self::official());
        }

        let raw = fixture_base.with_context(|| {
            format!("test update build requires {TEST_API_BASE_URL} to name its loopback fixture")
        })?;
        let mut api_base = Url::parse(raw).context("test update source is not a valid URL")?;
        ensure!(
            matches!(api_base.scheme(), "http" | "https"),
            "test update source must use HTTP or HTTPS"
        );
        ensure!(
            api_base.username().is_empty() && api_base.password().is_none(),
            "test update source must not contain userinfo"
        );
        ensure!(
            api_base.query().is_none() && api_base.fragment().is_none(),
            "test update source must not contain a query or fragment"
        );
        ensure!(
            api_base.path() == "/" || api_base.path().is_empty(),
            "test update source must have an empty path"
        );
        ensure!(
            matches!(
                api_base.host(),
                Some(Host::Ipv4(address)) if address.is_loopback()
            ) || matches!(api_base.host(), Some(Host::Ipv6(address)) if address.is_loopback())
                || api_base.host_str() == Some("localhost"),
            "test update source must use a loopback host"
        );
        api_base.set_path("/");
        Ok(Self {
            kind: UpdateSourceKind::TestFixture {
                api_base,
                verify_attestation,
            },
        })
    }

    pub(crate) fn is_fixture(&self) -> bool {
        matches!(&self.kind, UpdateSourceKind::TestFixture { .. })
    }

    pub(crate) fn verifies_attestation(&self) -> bool {
        match &self.kind {
            UpdateSourceKind::Official => true,
            UpdateSourceKind::TestFixture {
                verify_attestation, ..
            } => *verify_attestation,
        }
    }

    pub(crate) fn metadata_url(&self, repo: &str, suffix: &str) -> Result<Url> {
        let relative = format!("repos/{repo}/releases/{suffix}");
        match &self.kind {
            UpdateSourceKind::Official => Url::parse(&format!("{OFFICIAL_API_ORIGIN}/{relative}"))
                .context("compiled official update metadata URL is invalid"),
            UpdateSourceKind::TestFixture { api_base, .. } => api_base
                .join(&relative)
                .context("test update metadata URL is invalid"),
        }
    }

    pub(crate) fn expected_asset_url(&self, tag: &ReleaseTag, name: &str) -> Result<Url> {
        match &self.kind {
            UpdateSourceKind::Official => Url::parse(&format!(
                "{OFFICIAL_ASSET_ORIGIN}/{REPO}/releases/download/{tag}/{name}"
            ))
            .context("compiled official release asset URL is invalid"),
            UpdateSourceKind::TestFixture { api_base, .. } => api_base
                .join(name)
                .context("test release asset URL is invalid"),
        }
    }

    pub(crate) fn curl_protocols(&self) -> (&'static str, &'static str) {
        match &self.kind {
            UpdateSourceKind::Official => ("=https", "=https"),
            UpdateSourceKind::TestFixture { .. } => ("=http,https", "=http,https"),
        }
    }
}

pub(crate) fn asset_name(tag: &str, triple: &str) -> String {
    format!("coop-{tag}-{triple}.tar.gz")
}

pub(crate) fn normalize_tag(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix('v').unwrap_or(trimmed);
    Version::parse(body)
        .with_context(|| format!("--version {trimmed:?} is not a valid semver tag"))?;
    Ok(format!("v{body}"))
}

pub(crate) fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseTag {
    text: String,
    version: Version,
}

impl ReleaseTag {
    pub(crate) fn from_metadata(input: &str) -> Result<Self> {
        let normalized = normalize_tag(input)
            .with_context(|| format!("release metadata tag {input:?} has invalid grammar"))?;
        ensure!(
            normalized == input,
            "release metadata tag {input:?} is not canonical v-prefixed semver"
        );
        let version = Version::parse(strip_v(input))
            .with_context(|| format!("release metadata tag {input:?} has invalid grammar"))?;
        Ok(Self {
            text: input.to_string(),
            version,
        })
    }

    pub(crate) fn from_requested(input: &str) -> Result<Self> {
        let text = normalize_tag(input)?;
        let version = Version::parse(strip_v(&text))
            .context("normalized requested release tag is invalid")?;
        Ok(Self { text, version })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn version(&self) -> &Version {
        &self.version
    }
}

impl std::fmt::Display for ReleaseTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Release {
    #[serde(rename = "tag_name")]
    pub(crate) tag: String,
    #[serde(default)]
    pub(crate) assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Asset {
    pub(crate) name: String,
    #[serde(rename = "browser_download_url")]
    pub(crate) url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedAsset {
    source: UpdateSource,
    tag: ReleaseTag,
    name: String,
    url: Url,
}

impl ValidatedAsset {
    pub(crate) fn source(&self) -> &UpdateSource {
        &self.source
    }

    pub(crate) fn tag(&self) -> &ReleaseTag {
        &self.tag
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn url(&self) -> &Url {
        &self.url
    }
}

#[derive(Debug)]
pub(crate) struct ValidatedRelease {
    tag: ReleaseTag,
    tarball: ValidatedAsset,
    sums: ValidatedAsset,
    bundle: Option<ValidatedAsset>,
}

impl ValidatedRelease {
    pub(crate) fn tag(&self) -> &ReleaseTag {
        &self.tag
    }

    pub(crate) fn tarball(&self) -> &ValidatedAsset {
        &self.tarball
    }

    pub(crate) fn sums(&self) -> &ValidatedAsset {
        &self.sums
    }

    pub(crate) fn bundle(&self) -> Option<&ValidatedAsset> {
        self.bundle.as_ref()
    }
}

pub(crate) fn validate_release_tag(
    release: &Release,
    requested: Option<&ReleaseTag>,
) -> Result<ReleaseTag> {
    let returned = ReleaseTag::from_metadata(&release.tag)?;
    if let Some(requested) = requested {
        ensure!(
            returned == *requested,
            "release metadata tag mismatch: requested {requested}, received {returned}"
        );
    }
    Ok(returned)
}

pub(crate) fn validate_asset_identity(
    source: &UpdateSource,
    tag: &ReleaseTag,
    expected_name: &str,
    asset: &Asset,
) -> Result<ValidatedAsset> {
    ensure!(
        asset.name == expected_name,
        "release asset identity mismatch: expected name {expected_name:?}, received {:?}",
        asset.name
    );
    ensure!(
        !asset.url.contains('\\') && !asset.url.contains('%'),
        "release asset identity mismatch for {expected_name}: encoded or backslash path syntax is not allowed"
    );
    let url = Url::parse(&asset.url).with_context(|| {
        format!("release asset identity mismatch for {expected_name}: invalid URL")
    })?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "release asset identity mismatch for {expected_name}: userinfo is not allowed"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "release asset identity mismatch for {expected_name}: query and fragment are not allowed"
    );
    let expected = source.expected_asset_url(tag, expected_name)?;
    ensure!(
        asset.url == expected.as_str() && url == expected,
        "release asset identity mismatch for {expected_name}: expected {expected}, received {}",
        asset.url
    );
    Ok(ValidatedAsset {
        source: source.clone(),
        tag: tag.clone(),
        name: expected_name.to_string(),
        url,
    })
}

fn select_validated_asset(
    source: &UpdateSource,
    release: &Release,
    tag: &ReleaseTag,
    expected_name: &str,
    required: bool,
) -> Result<Option<ValidatedAsset>> {
    let mut matches = release
        .assets
        .iter()
        .filter(|asset| asset.name == expected_name);
    let Some(first) = matches.next() else {
        ensure!(
            !required,
            "release {tag} has no asset {expected_name}; refusing to continue"
        );
        return Ok(None);
    };
    let validated = validate_asset_identity(source, tag, expected_name, first)?;
    for duplicate in matches {
        let other = validate_asset_identity(source, tag, expected_name, duplicate)?;
        ensure!(
            other == validated,
            "release asset identity mismatch for {expected_name}: conflicting duplicate records"
        );
    }
    Ok(Some(validated))
}

pub(crate) fn validate_release_assets(
    source: &UpdateSource,
    release: &Release,
    requested: Option<&ReleaseTag>,
    triple: &str,
) -> Result<ValidatedRelease> {
    let tag = validate_release_tag(release, requested)?;
    let tarball_name = asset_name(tag.as_str(), triple);
    let tarball = select_validated_asset(source, release, &tag, &tarball_name, true)?
        .context("required target archive disappeared during validation")?;
    let sums = select_validated_asset(source, release, &tag, "SHA256SUMS", true)?
        .context("required checksum asset disappeared during validation")?;
    let bundle = select_validated_asset(source, release, &tag, BUNDLE_ASSET, false)?;
    Ok(ValidatedRelease {
        tag,
        tarball,
        sums,
        bundle,
    })
}

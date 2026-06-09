//! OCI-backed devcontainer Feature resolution.
//!
//! Devcontainer Features are registry artifacts that contain at least
//! `devcontainer-feature.json` and `install.sh`. Coop resolves a conservative
//! subset for setup-time image builds: public GHCR features under
//! `ghcr.io/devcontainers/features/*`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cmd::Cmd;
use crate::sha256_hash::Sha256Hash;

const GHCR_HOST: &str = "ghcr.io";
const SUPPORTED_REPO_PREFIX: &str = "devcontainers/features/";
const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.oci.artifact.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRequest {
    pub raw_id: String,
    pub reference: OciReference,
    pub options: BTreeMap<String, String>,
}

/// An OCI content digest restricted to the sha256 algorithm.
///
/// Parses and renders as `sha256:<64 lowercase hex>`. The algorithm
/// prefix is part of the wire form (it is what registry URLs and the
/// `@digest` reference syntax use), so it is preserved on `Display`; the
/// hex payload is validated once via [`Sha256Hash`] at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciDigest(Sha256Hash);

impl fmt::Display for OciDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.0)
    }
}

impl FromStr for OciDigest {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let hex = s
            .strip_prefix("sha256:")
            .with_context(|| format!("OCI digest must start with 'sha256:': {s:?}"))?;
        Ok(Self(hex.parse()?))
    }
}

impl Serialize for OciDigest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OciDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse()
            .map_err(|e: anyhow::Error| serde::de::Error::custom(format!("{e:#}")))
    }
}

/// How a feature pins its registry artifact: a mutable `:tag` or an
/// immutable `@sha256:` digest. The two have different syntax and
/// different guarantees (a digest is content-addressable), so they are
/// kept distinct rather than conflated in one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OciRef {
    Tag(String),
    Digest(OciDigest),
}

impl fmt::Display for OciRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag(tag) => f.write_str(tag),
            Self::Digest(digest) => write!(f, "{digest}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciReference {
    pub host: String,
    pub repository: String,
    pub reference: OciRef,
}

impl OciReference {
    #[must_use]
    pub fn canonical(&self) -> String {
        format!("{}/{}/{}", self.host, self.repository, self.reference)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstalledFeature {
    pub id: String,
    pub reference: String,
    pub digest: OciDigest,
    pub install_script_hash: Sha256Hash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFeature {
    pub installed: InstalledFeature,
    pub install_script: String,
    pub archive: Vec<u8>,
    pub options: BTreeMap<String, String>,
}

/// Project the persistable [`InstalledFeature`] records out of resolved
/// features, dropping the install scripts and archives that are not part
/// of the persisted template config.
pub fn installed_features(features: &[ResolvedFeature]) -> Vec<InstalledFeature> {
    features.iter().map(|f| f.installed.clone()).collect()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciManifest {
    #[serde(default)]
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Vec<Descriptor>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

struct FetchedManifest {
    manifest: OciManifest,
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    digest: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeatureMetadata {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

pub fn parse_feature_request(
    raw_id: &str,
    raw_options: &serde_json::Value,
) -> Result<Option<FeatureRequest>> {
    let Some(reference) = parse_supported_reference(raw_id)? else {
        return Ok(None);
    };
    let options = parse_options(raw_options)?;
    Ok(Some(FeatureRequest {
        raw_id: raw_id.to_string(),
        reference,
        options,
    }))
}

fn parse_supported_reference(raw_id: &str) -> Result<Option<OciReference>> {
    let without_scheme = raw_id
        .strip_prefix("oci://")
        .or_else(|| raw_id.strip_prefix("https://"))
        .unwrap_or(raw_id);
    let Some(rest) = without_scheme.strip_prefix("ghcr.io/") else {
        return Ok(None);
    };
    let Some(repo_and_ref) = rest.strip_prefix(SUPPORTED_REPO_PREFIX) else {
        return Ok(None);
    };
    let (name, reference) = if let Some((name, digest)) = repo_and_ref.split_once('@') {
        let digest: OciDigest = digest
            .parse()
            .with_context(|| format!("invalid feature digest in {raw_id:?}"))?;
        (name, OciRef::Digest(digest))
    } else if let Some((name, tag)) = repo_and_ref.rsplit_once(':') {
        if tag.is_empty() {
            bail!("expected ghcr.io/devcontainers/features/<name>[:tag|@digest]");
        }
        (name, OciRef::Tag(tag.to_string()))
    } else {
        (repo_and_ref, OciRef::Tag("latest".to_string()))
    };
    if name.is_empty() {
        bail!("expected ghcr.io/devcontainers/features/<name>[:tag|@digest]");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("feature name contains unsupported characters");
    }
    Ok(Some(OciReference {
        host: GHCR_HOST.to_string(),
        repository: format!("{SUPPORTED_REPO_PREFIX}{name}"),
        reference,
    }))
}

fn parse_options(raw: &serde_json::Value) -> Result<BTreeMap<String, String>> {
    let serde_json::Value::Object(map) = raw else {
        bail!("expected feature options object");
    };
    let mut out = BTreeMap::new();
    for (key, value) in map {
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("feature option {key:?} contains unsupported characters");
        }
        let rendered = match value {
            serde_json::Value::Bool(v) => v.to_string(),
            serde_json::Value::Number(v) => v.to_string(),
            serde_json::Value::String(v) => v.clone(),
            serde_json::Value::Null => String::new(),
            _ => bail!("feature option {key:?} must be a string, number, boolean, or null"),
        };
        out.insert(key.clone(), rendered);
    }
    Ok(out)
}

pub fn resolve_features(requests: &[FeatureRequest]) -> Vec<Result<ResolvedFeature>> {
    requests.iter().map(resolve_feature).collect()
}

fn resolve_feature(req: &FeatureRequest) -> Result<ResolvedFeature> {
    let token = fetch_token(&req.reference)?;
    let fetched = fetch_manifest(&req.reference, &token)?;
    let manifest = fetched.manifest;
    let metadata = feature_metadata(&manifest).unwrap_or_default();
    let blob = feature_blob(&manifest)?;
    let tmp = tempfile::tempdir().context("Failed to create feature extraction directory")?;
    let blob_path = tmp.path().join("feature.tgz");
    download_blob(&req.reference, &blob.digest, &token, &blob_path)?;
    resolved_feature_from_archive(req, &fetched.digest, metadata, &blob_path)
}

fn resolved_feature_from_archive(
    req: &FeatureRequest,
    manifest_digest: &str,
    metadata: FeatureMetadata,
    blob_path: &Path,
) -> Result<ResolvedFeature> {
    let id = metadata
        .id
        .or(metadata.name)
        .unwrap_or_else(|| req.raw_id.clone());
    let archive = fs::read(blob_path).with_context(|| {
        format!(
            "Failed to read downloaded feature layer {}",
            blob_path.display()
        )
    })?;
    let tmp = tempfile::tempdir().context("Failed to create feature validation directory")?;
    let extracted = tmp.path().join("feature");
    fs::create_dir(&extracted).context("Failed to create feature extraction target")?;
    Cmd::new("tar")
        .args(["-xzf"])
        .arg(blob_path)
        .args(["-C"])
        .arg(&extracted)
        .run()
        .context("Failed to extract devcontainer feature layer")?;
    let install_path = extracted.join("install.sh");
    let metadata_path = extracted.join("devcontainer-feature.json");
    if !metadata_path.exists() {
        bail!("feature layer does not contain devcontainer-feature.json");
    }
    let install_script = fs::read_to_string(&install_path)
        .with_context(|| format!("Failed to read {}", install_path.display()))?;
    if install_script.trim().is_empty() {
        bail!("feature install.sh is empty");
    }
    let digest: OciDigest = manifest_digest.parse().with_context(|| {
        format!("registry returned unexpected manifest digest {manifest_digest:?}")
    })?;
    Ok(ResolvedFeature {
        installed: InstalledFeature {
            id,
            reference: req.raw_id.clone(),
            digest,
            install_script_hash: Sha256Hash::of(&install_script),
        },
        install_script,
        archive,
        options: req.options.clone(),
    })
}

fn fetch_token(reference: &OciReference) -> Result<String> {
    let url = format!(
        "https://{GHCR_HOST}/token?scope=repository:{}:pull&service={GHCR_HOST}",
        reference.repository
    );
    let body = Cmd::new("curl")
        .args(["-fsSL"])
        .arg(url)
        .capture()
        .context("Failed to fetch GHCR registry token")?;
    let token: TokenResponse = serde_json::from_str(&body).context("Failed to parse token JSON")?;
    Ok(token.token)
}

fn fetch_manifest(reference: &OciReference, token: &str) -> Result<FetchedManifest> {
    let url = format!(
        "https://{GHCR_HOST}/v2/{}/manifests/{}",
        reference.repository, reference.reference
    );
    let tmp = tempfile::tempdir().context("Failed to create manifest download directory")?;
    let headers_path = tmp.path().join("headers");
    let body_path = tmp.path().join("manifest.json");
    Cmd::new("curl")
        .args(["-fsSL"])
        .args(["-D"])
        .arg(&headers_path)
        .args(["-o"])
        .arg(&body_path)
        .args(["-H"])
        .arg(format!("Accept: {MANIFEST_ACCEPT}"))
        .redacted_arg("-H")
        .redacted_arg(format!("Authorization: Bearer {token}"))
        .arg(url)
        .run()
        .with_context(|| {
            format!(
                "Failed to fetch feature manifest for {}",
                reference.canonical()
            )
        })?;
    let headers = fs::read_to_string(&headers_path)
        .with_context(|| format!("Failed to read {}", headers_path.display()))?;
    let body = fs::read_to_string(&body_path)
        .with_context(|| format!("Failed to read {}", body_path.display()))?;
    let manifest: OciManifest =
        serde_json::from_str(&body).context("Failed to parse OCI manifest")?;
    let digest = match parse_docker_content_digest(&headers) {
        Some(digest) => digest,
        None => manifest_digest(&manifest)?,
    };
    Ok(FetchedManifest { manifest, digest })
}

fn download_blob(reference: &OciReference, digest: &str, token: &str, output: &Path) -> Result<()> {
    let url = format!(
        "https://{GHCR_HOST}/v2/{}/blobs/{digest}",
        reference.repository
    );
    Cmd::new("curl")
        .args(["-fsSL"])
        .redacted_arg("-H")
        .redacted_arg(format!("Authorization: Bearer {token}"))
        .arg(url)
        .args(["-o"])
        .arg(output)
        .run()
        .with_context(|| format!("Failed to download feature layer {digest}"))
}

fn manifest_digest(manifest: &OciManifest) -> Result<String> {
    if let Some(digest) = manifest
        .annotations
        .get("org.opencontainers.image.digest")
        .or_else(|| manifest.annotations.get("dev.containers.digest"))
    {
        return Ok(digest.clone());
    }
    if let Some(config) = &manifest.config {
        return Ok(config.digest.clone());
    }
    bail!("OCI manifest has no digest-bearing config descriptor")
}

fn parse_docker_content_digest(headers: &str) -> Option<String> {
    headers.lines().rev().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("docker-content-digest") {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

fn feature_blob(manifest: &OciManifest) -> Result<&Descriptor> {
    manifest
        .layers
        .iter()
        .find(|d| {
            d.annotations
                .get("org.opencontainers.image.title")
                .is_some_and(|title| {
                    Path::new(title).extension().is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("tgz") || ext.eq_ignore_ascii_case("gz")
                    })
                })
        })
        .or_else(|| manifest.layers.first())
        .context("OCI manifest has no downloadable feature layer")
}

fn feature_metadata(manifest: &OciManifest) -> Option<FeatureMetadata> {
    manifest
        .annotations
        .get("dev.containers.metadata")
        .and_then(|raw| serde_json::from_str(raw).ok())
}

pub fn compose_install_snippet(feature: &ResolvedFeature) -> String {
    let mut out = String::new();
    out.push_str("\necho '  [guest] Installing devcontainer Feature ");
    out.push_str(&shell_single_quote(&feature.installed.reference));
    out.push_str("'\n");
    out.push_str("(\n");
    out.push_str("feature_dir=$(mktemp -d)\n");
    out.push_str("trap 'rm -rf \"$feature_dir\"' EXIT\n");
    out.push_str("archive=\"$feature_dir/feature.tgz\"\n");
    let archive_delimiter = format!(
        "COOP_FEATURE_ARCHIVE_{}",
        &feature.installed.install_script_hash.to_string()[..16]
    );
    out.push_str("base64 -d > \"$archive\" <<'");
    out.push_str(&archive_delimiter);
    out.push_str("'\n");
    out.push_str(&base64_encode(&feature.archive));
    out.push('\n');
    out.push_str(&archive_delimiter);
    out.push('\n');
    out.push_str("tar -xzf \"$archive\" -C \"$feature_dir\"\n");
    out.push_str("chmod +x \"$feature_dir/install.sh\"\n");
    out.push_str("export _REMOTE_USER=\"$GUEST_USER\"\n");
    out.push_str("export _REMOTE_USER_HOME=\"/home/$GUEST_USER\"\n");
    for (key, value) in &feature.options {
        out.push_str("export ");
        out.push_str(&option_env_name(key));
        out.push('=');
        out.push_str(&shell_single_quote(value));
        out.push('\n');
    }
    out.push_str("cd \"$feature_dir\"\n");
    out.push_str("./install.sh\n");
    out.push_str(")\n");
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn option_env_name(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests use unwrap for brevity")]
mod tests {
    use super::*;

    // A syntactically valid sha256 OCI digest for fixtures.
    const SAMPLE_DIGEST: &str =
        "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    #[test]
    fn parse_supported_reference_defaults_to_latest() {
        let req = parse_feature_request(
            "ghcr.io/devcontainers/features/github-cli",
            &serde_json::json!({}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            req.reference.repository,
            "devcontainers/features/github-cli"
        );
        assert_eq!(req.reference.reference, OciRef::Tag("latest".to_string()));
    }

    #[test]
    fn parse_supported_reference_accepts_tag() {
        let req = parse_feature_request(
            "ghcr.io/devcontainers/features/go:1.2.3",
            &serde_json::json!({ "version": "1.24", "moby": true }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(req.reference.reference, OciRef::Tag("1.2.3".to_string()));
        assert_eq!(req.options["version"], "1.24");
        assert_eq!(req.options["moby"], "true");
    }

    #[test]
    fn parse_rejects_nested_options() {
        let err = parse_feature_request(
            "ghcr.io/devcontainers/features/go:1",
            &serde_json::json!({ "version": { "nested": true } }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn compose_install_snippet_exports_options_and_script() {
        let feature = ResolvedFeature {
            installed: InstalledFeature {
                id: "github-cli".to_string(),
                reference: "ghcr.io/devcontainers/features/github-cli:1".to_string(),
                digest: SAMPLE_DIGEST.parse().unwrap(),
                install_script_hash: Sha256Hash::of("echo install"),
            },
            install_script: "echo \"$VERSION\"".to_string(),
            archive: b"archive-bytes".to_vec(),
            options: BTreeMap::from([("version".to_string(), "latest".to_string())]),
        };
        let snippet = compose_install_snippet(&feature);
        assert!(snippet.contains("export VERSION='latest'"));
        assert!(snippet.contains("YXJjaGl2ZS1ieXRlcw=="));
        assert!(snippet.contains("tar -xzf \"$archive\""));
        assert!(snippet.contains("./install.sh"));
    }

    #[test]
    fn base64_encoder_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"archive-bytes"), "YXJjaGl2ZS1ieXRlcw==");
    }

    #[test]
    fn parses_sample_manifest_metadata() {
        let manifest: OciManifest = serde_json::from_value(serde_json::json!({
            "config": { "digest": "sha256:config" },
            "layers": [
                {
                    "digest": "sha256:layer",
                    "annotations": { "org.opencontainers.image.title": "devcontainer-feature-go.tgz" }
                }
            ],
            "annotations": {
                "dev.containers.metadata": "{\"id\":\"go\",\"name\":\"Go\"}"
            }
        }))
        .unwrap();
        assert_eq!(manifest_digest(&manifest).unwrap(), "sha256:config");
        assert_eq!(feature_blob(&manifest).unwrap().digest, "sha256:layer");
        assert_eq!(
            feature_metadata(&manifest).unwrap().id.as_deref(),
            Some("go")
        );
    }

    #[test]
    fn resolves_sample_feature_archive_with_payload_files() {
        let tmp = tempfile::tempdir().unwrap();
        let feature_dir = tmp.path().join("feature");
        std::fs::create_dir(&feature_dir).unwrap();
        std::fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{ "id": "sample", "name": "Sample" }"#,
        )
        .unwrap();
        std::fs::write(feature_dir.join("helper.sh"), "echo helper\n").unwrap();
        std::fs::write(feature_dir.join("install.sh"), "#!/bin/sh\n. ./helper.sh\n").unwrap();
        let archive = tmp.path().join("feature.tgz");
        let status = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .args(["-C"])
            .arg(&feature_dir)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());

        let req = FeatureRequest {
            raw_id: "ghcr.io/devcontainers/features/sample:1".to_string(),
            reference: OciReference {
                host: GHCR_HOST.to_string(),
                repository: "devcontainers/features/sample".to_string(),
                reference: OciRef::Tag("1".to_string()),
            },
            options: BTreeMap::new(),
        };
        let resolved = resolved_feature_from_archive(
            &req,
            SAMPLE_DIGEST,
            FeatureMetadata {
                id: Some("sample".to_string()),
                name: None,
            },
            &archive,
        )
        .unwrap();

        assert_eq!(resolved.installed.id, "sample");
        assert_eq!(resolved.installed.digest.to_string(), SAMPLE_DIGEST);
        assert!(resolved.install_script.contains("./helper.sh"));
        assert_eq!(resolved.archive, std::fs::read(&archive).unwrap());
        assert!(compose_install_snippet(&resolved).contains(&base64_encode(&resolved.archive)));
    }

    #[test]
    fn parses_registry_manifest_digest_header() {
        let headers = "HTTP/2 200\r\ndocker-content-digest: sha256:manifest\r\n\r\n";
        assert_eq!(
            parse_docker_content_digest(headers).as_deref(),
            Some("sha256:manifest")
        );
    }

    #[test]
    fn oci_digest_round_trips_with_prefix() {
        let digest: OciDigest = SAMPLE_DIGEST.parse().unwrap();
        assert_eq!(digest.to_string(), SAMPLE_DIGEST);
    }

    #[test]
    fn oci_digest_rejects_missing_prefix() {
        let bare = SAMPLE_DIGEST.strip_prefix("sha256:").unwrap();
        assert!(bare.parse::<OciDigest>().is_err());
    }

    #[test]
    fn oci_digest_rejects_bad_hex() {
        assert!("sha256:not-hex".parse::<OciDigest>().is_err());
        assert!("sha256:abc".parse::<OciDigest>().is_err());
    }

    #[test]
    fn oci_digest_serde_round_trips_as_string() {
        let digest: OciDigest = SAMPLE_DIGEST.parse().unwrap();
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"{SAMPLE_DIGEST}\""));
        let parsed: OciDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, digest);
    }

    #[test]
    fn parse_supported_reference_accepts_digest() {
        let req = parse_feature_request(
            &format!("ghcr.io/devcontainers/features/go@{SAMPLE_DIGEST}"),
            &serde_json::json!({}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            req.reference.reference,
            OciRef::Digest(SAMPLE_DIGEST.parse().unwrap())
        );
    }

    #[test]
    fn parse_supported_reference_rejects_malformed_digest() {
        let err = parse_feature_request(
            "ghcr.io/devcontainers/features/go@sha256:nope",
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("digest"),
            "expected digest error, got: {err}"
        );
    }

    #[test]
    fn oci_ref_display_matches_wire_form() {
        assert_eq!(OciRef::Tag("1.2.3".to_string()).to_string(), "1.2.3");
        let digest: OciDigest = SAMPLE_DIGEST.parse().unwrap();
        assert_eq!(OciRef::Digest(digest).to_string(), SAMPLE_DIGEST);
    }
}

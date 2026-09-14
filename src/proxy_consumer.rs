//! Validated public card feed. This module never reads private storage or source data.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_public_base_url(value: &str) -> Result<String> {
    let value = value.trim_end_matches('/');
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "https"
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "invalid public asset base URL"
    );
    Ok(value.to_owned())
}

const PROXY_SCHEMA_KEY: &str = "catalogs/v1/consumers/proxy/schema-1.json";
const PROXY_PROFILE: &str = "proxy-square-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaceKind {
    Single,
    Multiple,
    Variants,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaceMetadata {
    pub id: String,
    pub title: String,
    /// None for single-sided cards; one-based for reverse faces or variants.
    pub selector: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrintingMetadata {
    /// Stable pipeline identity, retained when provisional public numbers change.
    pub id: String,
    pub card_id: String,
    pub upstream_card_id: Option<String>,
    pub printing_id: u32,
    pub print_group: String,
    pub printing_name: String,
    pub title: String,
    pub cycle: Label,
    pub set: Label,
    pub language: String,
    pub face_kind: FaceKind,
    pub faces: Vec<FaceMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyImage {
    pub face_id: String,
    pub sha256: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub media_type: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyPrinting {
    pub metadata: PrintingMetadata,
    pub images: Vec<ProxyImage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyIndex {
    pub schema: u32,
    pub schema_url: String,
    pub revision: String,
    pub profile: String,
    pub visibility: String,
    pub printing: Vec<ProxyPrinting>,
}

fn validate_metadata(printings: &[ProxyPrinting]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    let mut cards = BTreeMap::new();
    for printing in printings {
        let item = &printing.metadata;
        for value in [
            &item.id,
            &item.card_id,
            &item.print_group,
            &item.printing_name,
            &item.title,
            &item.cycle.id,
            &item.cycle.name,
            &item.set.id,
            &item.set.name,
        ] {
            ensure!(
                !value.trim().is_empty() && !value.chars().any(char::is_control),
                "metadata contains an empty or control-character field"
            );
        }
        ensure!(item.printing_id > 0, "printing number must be positive");
        ensure!(
            item.upstream_card_id
                .as_ref()
                .is_none_or(|id| !id.trim().is_empty()),
            "empty upstream card identity"
        );
        item.language
            .parse::<unic_langid::LanguageIdentifier>()
            .context("invalid metadata language")?;
        ensure!(ids.insert(&item.id), "duplicate pipeline printing identity");
        let identity = (
            &item.title,
            &item.upstream_card_id,
            item.face_kind,
            item.faces
                .iter()
                .map(|face| &face.title)
                .collect::<Vec<_>>(),
        );
        if let Some(previous) = cards.insert((&item.card_id, &item.language), identity.clone()) {
            ensure!(
                previous == identity,
                "conflicting metadata for one card identity"
            );
        }
        match item.face_kind {
            FaceKind::Single => ensure!(
                item.faces.len() == 1 && item.faces[0].selector.is_none(),
                "single card requires one face without a selector"
            ),
            FaceKind::Multiple | FaceKind::Variants => {
                ensure!(
                    item.faces.len() >= 2,
                    "multiple faces require at least two faces"
                );
                for (i, face) in item.faces.iter().enumerate() {
                    ensure!(
                        face.selector.map(usize::from) == Some(i + 1),
                        "face selectors must be consecutive and one-based"
                    );
                }
            }
        }
        ensure!(
            item.faces[0].title == item.title,
            "front title differs from card title"
        );
        let mut faces = BTreeSet::new();
        for face in &item.faces {
            ensure!(
                !face.id.trim().is_empty() && !face.title.trim().is_empty(),
                "empty face identity or title"
            );
            ensure!(faces.insert(&face.id), "duplicate metadata face");
            ensure!(
                aliases.insert((item.printing_id, &item.print_group, face.selector)),
                "duplicate public printing alias"
            );
        }
    }
    Ok(())
}

pub fn parse_proxy_index(bytes: &[u8], base_url: &str) -> Result<ProxyIndex> {
    let index: ProxyIndex = serde_json::from_slice(bytes).context("invalid proxy index JSON")?;
    validate_proxy_index(&index, base_url)?;
    Ok(index)
}

fn validate_proxy_index(index: &ProxyIndex, base_url: &str) -> Result<()> {
    let base = validate_public_base_url(base_url)?;
    ensure!(
        index.schema == 1 && index.profile == PROXY_PROFILE && index.visibility == "public",
        "unsupported proxy index contract"
    );
    ensure!(
        index.schema_url == format!("{base}/{PROXY_SCHEMA_KEY}"),
        "unexpected proxy schema URL"
    );
    let mut expected = index.clone();
    expected
        .printing
        .sort_by(|a, b| a.metadata.id.cmp(&b.metadata.id));
    expected.revision.clear();
    expected.revision = sha256(&serde_json::to_vec(&expected)?);
    ensure!(
        index == &expected,
        "proxy index revision or ordering mismatch"
    );
    validate_metadata(&index.printing)?;
    for printing in &index.printing {
        ensure!(
            printing.images.len() == printing.metadata.faces.len(),
            "incomplete proxy images"
        );
        for (face, image) in printing.metadata.faces.iter().zip(&printing.images) {
            ensure!(face.id == image.face_id, "proxy image face mismatch");
            ensure!(
                is_sha256(&image.sha256)
                    && image.bytes > 0
                    && image.width == 750
                    && image.height == 1050
                    && image.media_type == "image/webp",
                "invalid full-size proxy WebP image"
            );
            ensure!(
                image.url
                    == format!(
                        "{base}/assets/v1/sha256/{}/{}.webp",
                        &image.sha256[..2],
                        image.sha256
                    ),
                "proxy image URL is not the expected immutable public asset"
            );
        }
    }
    Ok(())
}

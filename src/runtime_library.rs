//! Assemble one consistent library and image map before making either visible.
use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result, ensure};

use crate::{
    ActiveState, AlternateFaceMetadata, CardFacePrintingId, CardId, CardMetadata, Library,
    MultiLibrary, PrintingMetadata, PublishedAssetIndex, Title,
    proxy_consumer::{FaceKind, parse_proxy_index},
};

fn title(text: &str) -> Title {
    Title {
        title: text.to_owned(),
        stripped_title: text.chars().filter(char::is_ascii).collect(),
    }
}

/// Always start from the bundled library, so old dynamic aliases disappear on
/// correction or withdrawal while bundled and local data survive.
pub fn load_proxy_index(base: &MultiLibrary, json: &str, asset_base: &str) -> Result<ActiveState> {
    let index = parse_proxy_index(json.as_bytes(), asset_base)?;
    let mut library = base.clone();
    let mut dynamic_assets = HashMap::new();
    for entry in &index.printing {
        let meta = &entry.metadata;
        let group = library
            .libraries
            .entry(meta.print_group.clone())
            .or_insert_with(|| Library {
                cards: HashMap::new(),
                faces: HashMap::new(),
                inserts: HashMap::new(),
            });
        library
            .collection_names
            .entry(meta.print_group.clone())
            .or_insert_with(|| meta.print_group.clone());
        let aliases = meta
            .faces
            .iter()
            .map(|face| CardFacePrintingId {
                id: meta.printing_id,
                face_or_variant_specifier: face.selector.map(usize::from),
                print_group: meta.print_group.clone(),
            })
            .collect::<Vec<_>>();
        let known = aliases
            .iter()
            .filter_map(|alias| group.faces.get(alias))
            .collect::<Vec<_>>();
        let identities = known.iter().map(|p| &p.card_id).collect::<BTreeSet<_>>();
        ensure!(
            identities.len() <= 1,
            "public aliases refer to different cards"
        );
        let stable = CardId(meta.card_id.clone());
        let card_id = if let Some(upstream) = &meta.upstream_card_id {
            CardId(upstream.clone())
        } else if group.cards.contains_key(&stable) {
            stable.clone()
        } else if known.len() == aliases.len() {
            known[0].card_id.clone()
        } else {
            ensure!(
                known.is_empty(),
                "cannot infer an upstream identity from incomplete face aliases"
            );
            stable.clone()
        };
        ensure!(
            known.iter().all(|p| p.card_id == card_id),
            "public printing alias conflicts with card identity"
        );
        ensure!(
            card_id == stable || !group.cards.contains_key(&stable),
            "explicit upstream identity would duplicate an existing card"
        );
        let alternate_face_data = match meta.face_kind {
            FaceKind::Single => AlternateFaceMetadata::Single,
            FaceKind::Multiple => AlternateFaceMetadata::Multiple(
                meta.faces.iter().skip(1).map(|f| title(&f.title)).collect(),
            ),
            FaceKind::Variants => AlternateFaceMetadata::Variants(meta.faces.len()),
        };
        let card_title = title(&meta.title);
        if let Some(card) = group.cards.get(&card_id) {
            // Compare exact display text, not upstream normalization choices.
            let same_faces = match (&card.alternate_face_data, &alternate_face_data) {
                (AlternateFaceMetadata::Single, AlternateFaceMetadata::Single) => true,
                (AlternateFaceMetadata::Variants(a), AlternateFaceMetadata::Variants(b)) => a == b,
                (AlternateFaceMetadata::Multiple(a), AlternateFaceMetadata::Multiple(b)) => {
                    a.iter().map(|t| &t.title).eq(b.iter().map(|t| &t.title))
                }
                _ => false,
            };
            ensure!(
                card.title.title == card_title.title && same_faces,
                "published metadata conflicts with existing card or face titles"
            );
        }
        let card = group
            .cards
            .entry(card_id.clone())
            .or_insert_with(|| CardMetadata {
                id: card_id.clone(),
                title: card_title,
                alternate_face_data,
                printings: BTreeSet::new(),
            });
        for (alias, image) in aliases.into_iter().zip(&entry.images) {
            card.printings.insert(alias.clone());
            group.faces.insert(
                alias.clone(),
                PrintingMetadata {
                    id: alias.clone(),
                    card_id: card_id.clone(),
                    printing_name: meta.printing_name.clone(),
                },
            );
            ensure!(
                dynamic_assets.insert(alias, image.url.clone()).is_none(),
                "duplicate dynamic image alias"
            );
        }
    }
    let published = PublishedAssetIndex {
        card_faces: dynamic_assets,
        ..PublishedAssetIndex::default()
    };
    Ok(ActiveState { library, published })
}

/// Missing saved aliases never fall back to a guessed legacy image URL.
pub fn slot_available(slot: &crate::FilledCardSlot, library: &MultiLibrary) -> bool {
    match slot {
        crate::FilledCardSlot::Card { printing, card_id } => library
            .libraries
            .get(&printing.print_group)
            .and_then(|group| group.faces.get(printing))
            .is_some_and(|meta| {
                card_id
                    .as_ref()
                    .is_none_or(|expected| expected == &meta.card_id)
            }),
        crate::FilledCardSlot::Insert { insert } => library
            .libraries
            .get(&insert.print_group)
            .is_some_and(|group| group.inserts.contains_key(insert)),
    }
}

pub fn validate_print_list(slots: &[crate::FilledCardSlot], library: &MultiLibrary) -> Result<()> {
    ensure!(!slots.is_empty(), "add a card before printing");
    slots
        .iter()
        .find(|slot| !slot_available(slot, library))
        .map_or(Ok(()), |_| {
            Err(anyhow::anyhow!(
                "A saved printing is unavailable. Remove it and choose a current printing."
            ))
        })
        .context("cannot print this selection")
}

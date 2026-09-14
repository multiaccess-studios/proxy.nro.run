use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    sync::{Arc, Mutex},
};

use codee::{Decoder, Encoder};
use futures::{StreamExt, stream::FuturesUnordered};
use image::{DynamicImage, ImageReader, imageops::overlay};
use leptos::{
    html::{Button, Dialog},
    leptos_dom::logging::{console_error, console_log, console_warn},
    prelude::*,
    task::spawn_local,
};
use leptos_use::on_click_outside;
use leptos_use::storage::use_session_storage;
use nucleo_matcher::{
    Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};
use printpdf::{
    ImageCompression, ImageOptimizationOptions, LinePoint, Mm, Op, PaintMode, PdfDocument, PdfPage,
    PdfSaveOptions, Point, Polygon, PolygonRing, RawImage, WindingOrder, XObjectTransform,
};
use proxy_elev::{
    ACTIVE_STATE, AlternateFaceMetadata, BleedMode, CARD_ASSET_CATALOG_URL, CardFacePrintingId,
    CardId, CutIndicator, FilledCardSlot, InsertId, Library, MultiLibrary, PROXY_ASSET_BASE_URL,
    PROXY_CARD_INDEX_URL, PrintConfig, PrintFile, PrintSize,
};
use reactive_stores::{Store, Subfield};
use regex::Regex;
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Blob, Url, js_sys::Uint8Array};

fn normalize_request_url(url: &str) -> String {
    if url.starts_with('/') {
        if let Some(window) = web_sys::window() {
            if let Ok(origin) = window.location().origin() {
                return format!("{origin}{url}");
            }
        }
    }
    url.to_string()
}

fn with_library<R>(f: impl FnOnce(&MultiLibrary) -> R) -> R {
    let state = ACTIVE_STATE.read().expect("library lock");
    f(&state.library)
}

fn log_error<T>(result: anyhow::Result<T>) -> Option<T> {
    result
        .map_err(|error| console_warn(&format!("{error:#}")))
        .ok()
}

#[derive(Clone, Copy)]
struct PrintFeedback {
    print_error: RwSignal<Option<String>>,
}

async fn fetch_card_text(url: &str) -> anyhow::Result<Option<String>> {
    use anyhow::{bail, ensure};
    const LIMIT: usize = 16 * 1024 * 1024;
    let fetch = async {
        let url = normalize_request_url(url);
        let request = reqwest::Client::new().get(url);
        #[cfg(target_arch = "wasm32")]
        let request = request.fetch_credentials_omit();
        let response = request.send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status()?;
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= LIMIT as u64),
            "card feed is too large"
        );
        let bytes = response.bytes().await?;
        ensure!(bytes.len() <= LIMIT, "card feed is too large");
        Ok(Some(String::from_utf8(bytes.to_vec())?))
    };
    let deadline = gloo_timers::future::TimeoutFuture::new(15_000);
    futures::pin_mut!(fetch, deadline);
    match futures::future::select(fetch, deadline).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => bail!("card feed request timed out"),
    }
}

fn load_card_data(
    loading: RwSignal<bool>,
    library_version: Subfield<Store<AppState>, AppState, u32>,
) {
    spawn_local(async move {
        let (proxy, general, local) = futures::join!(
            fetch_card_text(PROXY_CARD_INDEX_URL),
            fetch_card_text(CARD_ASSET_CATALOG_URL),
            async {
                match fetch_card_text("/local-assets/manifest.local.ron").await {
                    Ok(None) => fetch_card_text("/manifest.local.ron").await,
                    result => result,
                }
            }
        );
        let mut state = proxy_elev::ActiveState {
            library: proxy_elev::MULTI_LIBRARY.clone(),
            published: proxy_elev::PublishedAssetIndex::default(),
        };
        if let Some(text) = log_error(proxy).flatten()
            && let Some(loaded) = log_error(proxy_elev::runtime_library::load_proxy_index(
                &state.library,
                &text,
                PROXY_ASSET_BASE_URL,
            ))
        {
            state = loaded;
        }
        if let Some(text) = log_error(general).flatten()
            && let Some(mut published) =
                log_error(proxy_elev::published_asset_index(&text, &state.library))
        {
            published.extend(state.published);
            state.published = published;
        }
        if let Some(text) = log_error(local).flatten()
            && let Some(overlay) =
                log_error(ron::from_str::<MultiLibrary>(&text).map_err(Into::into))
        {
            state.library.merge_overlay(overlay);
        }
        *ACTIVE_STATE.write().expect("library lock") = state;
        library_version.update(|version| *version += 1);
        loading.set(false);
    });
}

fn use_print_file() -> (Signal<PrintFile>, WriteSignal<PrintFile>) {
    let (get, set, _delete) = use_session_storage::<PrintFile, RonSerdeCodec>("print-set-v0");
    (get, set)
}

fn use_print_config() -> (Signal<PrintConfig>, WriteSignal<PrintConfig>) {
    let (get, set, _delete) = use_session_storage::<PrintConfig, RonSerdeCodec>("print-config-v0");
    (get, set)
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum OpenDialog {
    Edit(usize),
    Print,
    TtsExport,
    JnetImport,
    NrdbImport,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ImportStatus {
    Importing,
    CouldNotFind,
    Failed,
    InvalidFormat,
}

#[derive(Debug, Clone, Store)]
pub struct AppState {
    dialog: Option<OpenDialog>,
    printing: bool,
    import_status: Option<ImportStatus>,
    selected_library: String,
    library_version: u32,
}
fn use_open_dialog() -> Subfield<Store<AppState>, AppState, Option<OpenDialog>> {
    expect_context::<Store<AppState>>().dialog()
}
fn use_selected_library() -> Subfield<Store<AppState>, AppState, String> {
    expect_context::<Store<AppState>>().selected_library()
}
fn use_printing() -> Subfield<Store<AppState>, AppState, bool> {
    expect_context::<Store<AppState>>().printing()
}
fn use_import_status() -> Subfield<Store<AppState>, AppState, Option<ImportStatus>> {
    expect_context::<Store<AppState>>().import_status()
}
fn use_library_version() -> Subfield<Store<AppState>, AppState, u32> {
    expect_context::<Store<AppState>>().library_version()
}

pub struct RonSerdeCodec;

impl<T: Serialize> Encoder<T> for RonSerdeCodec {
    type Error = ron::Error;
    type Encoded = String;

    fn encode(val: &T) -> Result<Self::Encoded, Self::Error> {
        ron::to_string(val)
    }
}

impl<T> Decoder<T> for RonSerdeCodec
where
    for<'de> T: Deserialize<'de>,
{
    type Error = ron::error::SpannedError;
    type Encoded = str;

    fn decode(val: &Self::Encoded) -> Result<T, Self::Error> {
        ron::from_str(val)
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(Root);
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Libraries {
    loaded_libraries: HashSet<String>,
    library: Library,
}
impl Default for Libraries {
    fn default() -> Libraries {
        let mut base_state = Libraries {
            loaded_libraries: HashSet::new(),
            library: Library {
                cards: HashMap::new(),
                faces: HashMap::new(),
                inserts: HashMap::new(),
            },
        };
        let lib = with_library(|library| library.libraries["english"].clone());
        base_state.library.merge(&lib);
        base_state.loaded_libraries.insert("english".to_string());
        base_state
    }
}

#[component]
fn Root() -> impl IntoView {
    provide_context(Store::new(AppState {
        dialog: None,
        printing: false,
        import_status: None,
        selected_library: "english".to_string(),
        library_version: 0,
    }));
    let library_version = use_library_version();
    let loading = RwSignal::new(true);
    provide_context(PrintFeedback {
        print_error: RwSignal::new(None),
    });
    load_card_data(loading, library_version);
    view! {
        <div class="bg-zinc-900 grid auto-rows-[min-content_1fr_min-content] gap-2 h-screen"
            aria-busy=move || loading.get().to_string()>
            <InputLineNew />
            <div class="p-4 overflow-y-scroll">
                <DecklistView />
                <OpenDialog />
            </div>
            <div>
                <ControlConfig />
            </div>
        </div>
    }
}

#[component]
fn InputLineNew() -> impl IntoView {
    let (_, set_print_file) = use_print_file();
    let selected_library = use_selected_library();
    let library_version = use_library_version();
    let mut matcher_config = nucleo_matcher::Config::DEFAULT;
    matcher_config.ignore_case = true;
    matcher_config.normalize = true;
    matcher_config.prefer_prefix = true;
    let matcher = Arc::new(Mutex::new(Matcher::new(matcher_config)));

    let (input, set_input) = signal(String::new());

    let haystack = Memo::new(move |_| {
        let _ = library_version.get();
        let mut haystack = Haystack {
            haystack: vec![],
            mappings: HashMap::new(),
        };
        let library = with_library(|library| library.libraries[&selected_library.get()].clone());
        for (card, meta) in &library.cards {
            haystack.haystack.push(meta.title.title.clone());
            haystack.haystack.push(meta.title.stripped_title.clone());
            haystack
                .mappings
                .insert(meta.title.title.clone(), HaystackEntry::Card(card.clone()));
            haystack.mappings.insert(
                meta.title.stripped_title.clone(),
                HaystackEntry::Card(card.clone()),
            );
        }
        for (insert, meta) in &library.inserts {
            haystack.haystack.push(meta.title.title.clone());
            haystack.haystack.push(meta.title.stripped_title.clone());

            haystack.mappings.insert(
                meta.title.title.clone(),
                HaystackEntry::Insert(insert.clone()),
            );
            haystack.mappings.insert(
                meta.title.stripped_title.clone(),
                HaystackEntry::Insert(insert.clone()),
            );
            for group in &meta.insert_groups {
                haystack.haystack.push(group.clone());
                haystack
                    .mappings
                    .insert(group.clone(), HaystackEntry::InsertGroup(group.clone()));
            }
        }
        haystack
    });

    let found = Memo::new(move |_| {
        let haystack = haystack.read();
        let pattern = Pattern::parse(&input.get(), CaseMatching::Ignore, Normalization::Smart);
        let out = pattern.match_list(&haystack.haystack, &mut matcher.lock().unwrap());
        let mut found = HashSet::new();
        let mut olist = Vec::with_capacity(5);
        for (entry, _) in out {
            let entry = &haystack.mappings[entry];
            if found.insert(entry) {
                olist.push(entry.clone());
            }
            if found.len() == 5 {
                break;
            }
        }
        olist
    });

    let foundlist = move || {
        if input.read().is_empty() || found.get().is_empty() {
            view! {
                <>
                    <div
                        class="bg-slate-800 py-2 px-4 rounded-lg text-nowrap"
                    >
                        {"No matches found..."}
                    </div>
                </>
            }
            .into_any()
        } else {
            view! {
                <>
                    <For
                        each=move || found.get().into_iter().enumerate()
                        key=|(i, entry)| (*i, entry.clone())
                        children=move |(i, entry)| {
                            let name = match entry {
                                HaystackEntry::Card(card) => with_library(|library| {
                                    library.libraries[&selected_library.get()]
                                        .get_card(&card)
                                        .title
                                        .title
                                        .clone()
                                }),
                                HaystackEntry::Insert(insert) => with_library(|library| {
                                    library.libraries[&selected_library.get()]
                                        .get_insert(&insert)
                                        .title
                                        .title
                                        .clone()
                                }),
                                HaystackEntry::InsertGroup(group) => group.clone(),
                            };
                            let classes = if i == 0 {
                                "bg-blue-800 hover:bg-blue-600 py-2 px-4 rounded-lg cursor-pointer text-nowrap"
                            } else {
                                "bg-slate-800 hover:bg-slate-600 py-2 px-4 rounded-lg cursor-pointer text-nowrap"
                            };
                            view! {
                                <button
                                    type="button"
                                    on:click=move |_| {
                                        match &found.read()[i] {
                                            HaystackEntry::Card(card) => {
                                                let card = with_library(|library| {
                                                    library.libraries[&selected_library.get()]
                                                        .get_card(card)
                                                        .clone()
                                                });
                                                set_print_file.write().add_cards(&card);
                                            }
                                            HaystackEntry::Insert(insert) => {
                                                set_print_file.write().add_insert(insert.clone());
                                            }
                                            HaystackEntry::InsertGroup(group) => {
                                                let mut set_print_file = set_print_file.write();
                                                let inserts = with_library(|library| {
                                                    library.libraries[&selected_library.get()]
                                                        .inserts
                                                        .values()
                                                        .cloned()
                                                        .collect::<Vec<_>>()
                                                });
                                                for insert in inserts {
                                                    if insert.insert_groups.contains(group) {
                                                        set_print_file.add_insert(insert.id.clone());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    value={i}
                                    class={classes}
                                >
                                    {name}
                                </button>
                            }
                        }
                    />
                </>
            }.into_any()
        }
    };

    view! {
        <div class="bg-zinc-700 p-2 shadow-lg">
            <form
                class="grid grid-cols-[1fr_min-content] md:grid-cols-[min-content_1fr_min-content] gap-2"
                on:submit=move |v| {
                    v.prevent_default();
                    let found = found.read();
                    if found.is_empty() {
                        return;
                    }
                    match &found[0] {
                        HaystackEntry::Card(card) => {
                            let card = with_library(|library| {
                                library.libraries[&selected_library.get()]
                                    .get_card(card)
                                    .clone()
                            });
                            set_print_file.write().add_cards(&card);
                        }
                        HaystackEntry::Insert(insert) => {
                            set_print_file.write().add_insert(insert.clone());
                        }
                        HaystackEntry::InsertGroup(group) => {
                            let mut set_print_file = set_print_file.write();
                            let inserts = with_library(|library| {
                                library.libraries[&selected_library.get()]
                                    .inserts
                                    .values()
                                    .cloned()
                                    .collect::<Vec<_>>()
                            });
                            for insert in inserts {
                                if insert.insert_groups.contains(group) {
                                    set_print_file.add_insert(insert.id.clone());
                                }
                            }
                        }
                    }

                }
            >
                <select class="col-span-full md:col-[unset] px-2">
                    <option value="english">{"NSG English"}</option>
                    <option value="nsg" disabled>{"WIP"}</option>
                </select>
                <input
                    type="text"
                    on:input:target=move |ev| {
                        set_input.set(ev.target().value());
                    }
                    name="data-search"
                    class="bg-zinc-900 border-1 border-white py-2 px-4 rounded-md"
                    prop:value=input
                />
                <button
                    type="submit"
                    value="+"
                    class="bg-blue-800 hover:bg-blue-600 py-2 px-4 rounded-lg cursor-pointer font-bold text-lg"
                >{"+"}</button>
                <div
                    class="col-span-full flex gap-2 overflow-x-scroll"
                >
                    {foundlist}
                </div>
            </form>
        </div>
    }
}

#[component]
fn DecklistView() -> impl IntoView {
    let (print_file, _) = use_print_file();
    let open_dialog = use_open_dialog();
    let library_version = use_library_version();
    let num_items = Memo::new(move |_| print_file.with(PrintFile::len));
    view! {
        <div class="flex flex-wrap gap-2 justify-center">
            <For
                each=move || 0..num_items.get()
                key=|i| *i
                children=move |i| {
                    let card = Memo::new(move |_| {
                        print_file.with(|print_file| {
                            print_file.get(i).cloned()
                        })
                    });
                    let selected = Memo::new(move |_| {
                        open_dialog.get().is_some_and(|dialog| dialog == OpenDialog::Edit(i))
                    });
                    let name = Memo::new(move |_| card.with(|card| {
                        let _ = library_version.get();
                        card.as_ref().map(FilledCardSlot::name).unwrap_or_default()
                    }));
                    let image_url = Memo::new(move |_| card.with(|card| {
                        let _ = library_version.get();
                        card.as_ref().map(FilledCardSlot::image_url).unwrap_or_default()
                    }));
                    let is_local_override = Memo::new(move |_| {
                        let _ = library_version.get();
                        card.with(|card| {
                            card.as_ref()
                                .is_some_and(FilledCardSlot::is_local_override)
                        })
                    });
                    view! {
                        <button
                            class="relative"
                            on:click:target=move |_| {
                                open_dialog.set(Some(OpenDialog::Edit(i)));
                            }
                        >
                            <Show when=move || is_local_override.get()>
                                <span class="absolute top-1 right-1 z-10 text-[10px] font-bold bg-amber-500 text-black px-1 rounded">
                                    {"LOCAL"}
                                </span>
                            </Show>
                            <Show when=move || !image_url.get().is_empty() fallback=move || view! {
                                <span class="block w-40 p-2 border border-amber-500">{name}</span>
                            }>
                                <img class:ring-4=selected class="ring-blue-800 w-24 cursor-pointer" src=image_url alt=name />
                            </Show>
                        </button>
                    }
                }
            />
        </div>
    }
}

#[component]
fn OpenDialog() -> impl IntoView {
    let open_dialog = use_open_dialog();

    let modal_ref = NodeRef::<Dialog>::new();
    let _ = on_click_outside(modal_ref, move |_| open_dialog.set(None));

    move || {
        if let Some(open_dialog) = open_dialog.get() {
            view! {
                <div class="absolute top-0 left-0 w-full h-full bg-black/50 cursor-pointer" />
                <dialog
                    role="dialog"
                    class="absolute top-1/2 left-1/2 transform -translate-x-1/2 -translate-y-1/2 min-w-[min(80ch,80%)] p-4 flex flex-col gap-2 rounded-lg"
                    node_ref=modal_ref
                    open
                >
                    {move || match open_dialog {
                        OpenDialog::Edit(_) => view! { <DialogContentCard /> }.into_any(),
                        OpenDialog::Print => view! { <PrintContent /> }.into_any(),
                        OpenDialog::JnetImport => view! { <JnetImportContent /> }.into_any(),
                        OpenDialog::NrdbImport => view! { <NrdbImportContent /> }.into_any(),
                        OpenDialog::TtsExport => view! { <TtsExportContent /> }.into_any(),
                    }}
                </dialog>
            }.into_any()
        } else {
            view! {}.into_any()
        }
    }
}

#[component]
fn DialogContentCard() -> impl IntoView {
    let open_dialog = use_open_dialog();
    let (print_file, _) = use_print_file();
    let (_, set_print_file) = use_print_file();
    let library_version = use_library_version();

    let card = Memo::new(move |_| {
        let print_index = open_dialog.get();
        print_file.with(|print_file| {
            print_index.and_then(|index| match index {
                OpenDialog::Edit(index) => print_file.get(index).cloned(),
                _ => None,
            })
        })
    });

    let name = Memo::new(move |_| {
        let _ = library_version.get();
        card.with(|card| card.as_ref().map(FilledCardSlot::name).unwrap_or_default())
    });
    let is_local_override = Memo::new(move |_| {
        let _ = library_version.get();
        card.with(|card| card.as_ref().is_some_and(FilledCardSlot::is_local_override))
    });

    let face_info = Memo::new(move |_| {
        let _ = library_version.get();
        let Some(FilledCardSlot::Card {
            printing:
                ref face_id @ CardFacePrintingId {
                    face_or_variant_specifier: Some(face),
                    ref print_group,
                    ..
                },
            ..
        }) = card.get()
        else {
            return None;
        };
        let Some(card_data) = with_library(|library| {
            library
                .libraries
                .get(&*print_group)
                .and_then(|group| group.try_get_face_card(&face_id))
                .cloned()
        }) else {
            return None;
        };
        let faces = match &card_data.alternate_face_data {
            AlternateFaceMetadata::Single => return None,
            AlternateFaceMetadata::Multiple(titles) => titles.len() + 1,
            &AlternateFaceMetadata::Variants(n) => n,
        };
        Some((face, face_id.clone(), faces))
    });

    let face_edit = move || {
        let Some((face, face_id, faces)) = face_info.get() else {
            return ().into_any();
        };
        view! {
            <For
                each=move || 1..=faces
                key=|i| *i
                children=move |button_face| {
                    let face_id = face_id.clone();
                    let selected = button_face == face;
                    view! {
                        <button
                            class="hover:bg-zinc-600 p-2 rounded-lg cursor-pointer"
                            class:bg-blue-800=selected
                            class:bg-zinc-800=!selected
                            on:click:target=move |_| {
                                let mut new = face_id.clone();
                                new.face_or_variant_specifier = Some(button_face);
                                if let Some(OpenDialog::Edit(index)) = open_dialog.get() {
                                    set_print_file.write().update_card(index, new);
                                }
                            }
                        >
                            {"Face "} {button_face}
                        </button>
                    }
                }
            />
        }
        .into_any()
    };

    let modal_ref = NodeRef::<Dialog>::new();
    let _ = on_click_outside(modal_ref, move |_| open_dialog.set(None));

    let close_ref = NodeRef::<Button>::new();
    Effect::new(move |_| {
        if open_dialog.get().is_some() {
            if let Some(close_ref) = close_ref.get() {
                let _ = close_ref.focus();
            }
        }
    });

    move || {
        view! {
            <div class="flex justify-between items-start">
                <div class="flex items-center gap-2">
                    <p class="font-bold text-lg text-balance">{name}</p>
                    <Show when=move || is_local_override.get()>
                        <span class="text-[10px] font-bold bg-amber-500 text-black px-1 rounded">
                            {"LOCAL"}
                        </span>
                    </Show>
                </div>
                <button
                    node_ref=close_ref
                    class="cursor-pointer"
                    on:click:target=move |_| {
                        open_dialog.set(None);
                    }
                >
                    {"(close)"}
                </button>
            </div>
            <div class="flex gap-2 flex-wrap">
                <button
                    class="bg-red-800 hover:bg-red-600 p-2 rounded-lg cursor-pointer"
                    on:click:target=move |_| {
                        let (_, set_print_file) = use_print_file();
                        if let Some(OpenDialog::Edit(index)) = open_dialog.get() {
                            set_print_file.write().remove_card(index);
                        };
                        open_dialog.set(None);
                    }
                >
                    {"Remove"}
                </button>
                {face_edit}
            </div>
        }
        .into_any()
    }
}

#[component]
fn PrintContent() -> impl IntoView {
    let controls = expect_context::<PrintFeedback>();
    let print_error = controls.print_error;
    let (print_config, set_print_config) = use_print_config();
    let printing = use_printing();
    let sizes = [PrintSize::A4, PrintSize::UsLetter];
    let cut_indicators = [CutIndicator::Lines, CutIndicator::Marks, CutIndicator::None];
    let bleed_modes = [
        BleedMode::None,
        BleedMode::Narrow,
        BleedMode::Medium,
        BleedMode::Wide,
    ];
    let is_printing = Memo::new(move |_| printing.get());
    let is_not_printing = Memo::new(move |_| !is_printing.get());
    let print_message = Memo::new(move |_| {
        if is_printing.get() {
            "Generating..."
        } else {
            "Generate PDF"
        }
    });
    view! {
        <div class="flex flex-col gap-2 h-full justify-between">
            <p class="text-lg font-bold">{"Print"}</p>
            <Show when=move || print_error.get().is_some()>
                <p role="alert" class="bg-red-800 text-white p-2">{move || print_error.get().unwrap_or_default()}</p>
            </Show>
            <p class="bg-red-800 text-white font-bold px-2 py-1 max-w-max">
                {"Remember to disable any margin when printing!"}
            </p>
            <div class="flex gap-2 items-center flex-wrap">
                <div class="font-bold w-full md:w-[unset]">Paper Size</div>
                <For
                    each=move || sizes
                    key=|size| *size
                    children=move |size| {
                        let selected = Memo::new(move |_| {
                            print_config.with(|print_config| print_config.print_size == size)
                        });
                        let not_selected = Memo::new(move |_| !selected.get());
                        view! {
                            <button
                                class="p-2 rounded-lg cursor-pointer"
                                class:bg-blue-800=selected
                                class:hover:bg-zinc-600=not_selected
                                class:bg-zinc-800=not_selected
                                on:click:target=move |_| {
                                    set_print_config.update(move |config| config.print_size = size);
                                }
                            >
                                {format!("{size}")}
                            </button>
                        }
                    }
                />
            </div>
            <div class="flex gap-2 items-center flex-wrap">
                <div class="font-bold w-full md:w-[unset]">Cut Indicator</div>
                <For
                    each=move || cut_indicators
                    key=|cut| *cut
                    children=move |cut| {
                        let selected = Memo::new(move |_| {
                            print_config.with(|print_config| print_config.cut_indicator == cut)
                        });
                        let not_selected = Memo::new(move |_| !selected.get());
                        view! {
                            <button
                                class="p-2 rounded-lg cursor-pointer"
                                class:bg-blue-800=selected
                                class:hover:bg-zinc-600=not_selected
                                class:bg-zinc-800=not_selected
                                on:click:target=move |_| {
                                    set_print_config.update(move |config| config.cut_indicator = cut);
                                }
                            >
                                {format!("{cut}")}
                            </button>
                        }
                    }
                />
            </div>
            <div class="flex gap-2 items-center flex-wrap">
                <div class="font-bold w-full md:w-[unset]">{"Bleed Mode"}</div>
                <For
                    each=move || bleed_modes
                    key=|bleed| *bleed
                    children=move |bleed| {
                        let selected = Memo::new(move |_| {
                            print_config.with(|print_config| print_config.bleed_mode == bleed)
                        });
                        let not_selected = Memo::new(move |_| !selected.get());
                        view! {
                            <button
                                class="p-2 rounded-lg cursor-pointer"
                                class:bg-blue-800=selected
                                class:hover:bg-zinc-600=not_selected
                                class:bg-zinc-800=not_selected
                                on:click:target=move |_| {
                                    set_print_config.update(move |config| config.bleed_mode = bleed);
                                }
                            >
                                {format!("{bleed}")}
                            </button>
                        }
                    }
                />
            </div>
            <div>
                <button
                    class="bg-green-800 hover:bg-green-600 p-2 rounded-lg cursor-pointer font-bold text-lg"
                    class:bg-green-800=is_not_printing
                    class:hover:bg-green-600=is_not_printing
                    class:bg-red-800=is_printing
                    disabled=is_printing
                    on:click:target=move |_| {
                        do_print(printing, controls);
                    }
                >
                    {print_message}
                </button>
            </div>
        </div>
    }
}

#[component]
fn TtsExportContent() -> impl IntoView {
    let controls = expect_context::<PrintFeedback>();
    let print_error = controls.print_error;
    let printing = use_printing();
    let is_printing = Memo::new(move |_| printing.get());
    let is_not_printing = Memo::new(move |_| !is_printing.get());
    let print_message_corp = Memo::new(move |_| {
        if is_printing.get() {
            "Generating..."
        } else {
            "Export Corp Deck"
        }
    });
    let print_message_runner = Memo::new(move |_| {
        if is_printing.get() {
            "Generating..."
        } else {
            "Export Runner Deck"
        }
    });
    view! {
        <div class="flex flex-col gap-2 h-full justify-between">
            <p class="text-lg font-bold">{"Print"}</p>
            <Show when=move || print_error.get().is_some()>
                <p role="alert" class="bg-red-800 text-white p-2">{move || print_error.get().unwrap_or_default()}</p>
            </Show>
            <p class="bg-red-800 text-white font-bold px-2 py-1 w-max">
                <a href="https://www.google.com/search?hl=en&q=tts%20custom%20decklist%20import">
                    {"How to import into TTS?"}
                </a>
            </p>
            <p class="bg-blue-800 text-white font-bold px-2 py-1 max-w-max">
                {"Corp Back URL: "} <code class="wrap-break-word">{CORP_TTS_BACK}</code>
            </p>
            <p class="bg-blue-800 text-white font-bold px-2 py-1 max-w-max">
                {"Runner Back URL: "} <code class="wrap-break-word">{RUNNER_TTS_BACK}</code>
            </p>
            <div class="flex gap-2">
                <button
                    class="bg-green-800 hover:bg-green-600 p-2 rounded-lg cursor-pointer font-bold text-lg"
                    class:bg-green-800=is_not_printing
                    class:hover:bg-green-600=is_not_printing
                    class:bg-red-800=is_printing
                    disabled=is_printing
                    on:click:target=move |_| {
                        do_tts_export(CORP_TTS_BACK.to_string(), printing, controls);
                    }
                >
                    {print_message_corp}
                </button>
                <button
                    class="bg-green-800 hover:bg-green-600 p-2 rounded-lg cursor-pointer font-bold text-lg"
                    class:bg-green-800=is_not_printing
                    class:hover:bg-green-600=is_not_printing
                    class:bg-red-800=is_printing
                    disabled=is_printing
                    on:click:target=move |_| {
                        do_tts_export(RUNNER_TTS_BACK.to_string(), printing, controls);
                    }
                >
                    {print_message_runner}
                </button>
            </div>
        </div>
    }
}

#[component]
fn JnetImportContent() -> impl IntoView {
    let (text_content, set_text_content) = signal(String::new());
    let (_print_file, set_print_file) = use_print_file();
    view! {
        <p class="text-lg font-bold">{"JNET Import"}</p>
        <p class="bg-red-800 text-white font-bold px-2 py-1 w-max">
            {"Don't forgot to add your ID after!"}
        </p>
        <textarea
            class="bg-zinc-900 border-1 border-white p-2 rounded-md"
            prop:value=move || text_content.get()
            on:input:target=move |ev| set_text_content.set(ev.target().value())
        >
            {text_content.get_untracked()}
        </textarea>
        <button
            class="bg-blue-800 hover:bg-blue-600 p-2 rounded-lg cursor-pointer font-bold text-lg"
            on:click:target=move |_| {
                let text_content = text_content.get();
                'line: for line in text_content.lines() {
                    let Some((count, name)) = line.split_once(" ") else {
                        console_warn(&format!("Invalid Line: {line}"));
                        continue;
                    };
                    let Ok(count) = count.trim().parse::<usize>() else {
                        console_warn(&format!("Invalid Number: {count}"));
                        continue;
                    };
                    let cards = with_library(|library| {
                        library.libraries["english"]
                            .cards
                            .values()
                            .cloned()
                            .collect::<Vec<_>>()
                    });
                    for meta in cards {
                        if meta.title.title == name || meta.title.stripped_title == name {
                            for _ in 0..count {
                                set_print_file.write().add_cards(&meta);
                            }
                            continue 'line;
                        }
                    }
                    console_warn(&format!("Card not found: {name}"));
                }
            }
        >
            {"Import"}
        </button>
    }
}

#[component]
fn NrdbImportContent() -> impl IntoView {
    let (text_content, set_text_content) = signal(String::new());
    let import_status = use_import_status();
    let open_dialog = use_open_dialog();
    view! {
        <p class="text-lg font-bold">{"NRDB Import"}</p>
        <p class="bg-red-800 text-white font-bold px-2 py-1 w-max">
            {"Unsupported cards will be skipped!"}
        </p>
        <form
            class="contents"
            on:submit=move |ev| {
                ev.prevent_default();
                let text_content = text_content.get();
                do_nrdb_import(&text_content, import_status, open_dialog);
            }
        >
            <input
                type="text"
                class="bg-zinc-900 border-1 border-white p-2 rounded-md"
                prop:value=move || text_content.get()
                on:input:target=move |ev| set_text_content.set(ev.target().value())
            />
            <button
                class="bg-blue-800 hover:bg-blue-600 p-2 rounded-lg cursor-pointer font-bold text-lg"
            >
                {"Import"}
            </button>
        </form>
    }
}

#[component]
fn ControlConfig() -> impl IntoView {
    let open_dialog = use_open_dialog();
    let (_, set_print_file) = use_print_file();
    let (print_file, _) = use_print_file();

    let used_slots = Memo::new(move |_| print_file.read().len());
    let total_pages = Memo::new(move |_| used_slots.get().div_ceil(9));
    let overflow = Memo::new(move |_| {
        let slots = used_slots.get();
        let mut modu = slots % 9;
        if modu == 0 && slots > 0 {
            modu = 9;
        }
        modu
    });
    view! {
        <div class="flex flex-wrap gap-2 py-2 px-4 bg-zinc-700 items-center justify-between">
            <p class="font-bold text-lg">{"Proxy.NRO"}</p>
            <div class="flex flex-wrap gap-2 items-center">
                <p>{used_slots}{"c "}{total_pages}{"p ("}{overflow}{")"}</p>
                <button
                    class="bg-green-800 hover:bg-green-600 p-2 rounded-lg cursor-pointer"
                    on:click:target=move |_| {
                        open_dialog.set(Some(OpenDialog::Print));
                    }
                >
                    {"Print"}
                </button>
                <button
                    class="bg-green-800 hover:bg-green-600 p-2 rounded-lg cursor-pointer"
                    on:click:target=move |_| {
                        open_dialog.set(Some(OpenDialog::TtsExport));
                    }
                >
                    {"TTS"}
                </button>
                <button
                    class="bg-zinc-800 hover:bg-zinc-600 p-2 rounded-lg cursor-pointer"
                    on:click:target=move |_| {
                        open_dialog.set(Some(OpenDialog::JnetImport));
                    }
                >
                    {"JNET"}
                </button>
                <button
                    class="bg-zinc-800 hover:bg-zinc-600 p-2 rounded-lg cursor-pointer"
                    on:click:target=move |_| {
                        open_dialog.set(Some(OpenDialog::NrdbImport));
                    }
                >
                    {"NRDB"}
                </button>
                <button
                    class="bg-red-800 hover:bg-red-600 p-2 rounded-lg cursor-pointer"
                    on:click:target=move |_| {
                        set_print_file.write().clear();
                    }
                >
                    {"Clear"}
                </button>
            </div>
        </div>
    }
}

const CORP_TTS_BACK: &str = "https://nro-public.s3.nl-ams.scw.cloud/voluntary/public-assets/custom-assets/tts_card_backs/tts_corp_back.png";
const RUNNER_TTS_BACK: &str = "https://nro-public.s3.nl-ams.scw.cloud/voluntary/public-assets/custom-assets/tts_card_backs/tts_runner_back.png";

fn do_tts_export(
    back: String,
    printing: Subfield<Store<AppState>, AppState, bool>,
    controls: PrintFeedback,
) {
    controls.print_error.set(None);
    let (print_file, _) = use_print_file();
    let print_file = print_file.get_untracked();
    let valid = with_library(|library| {
        proxy_elev::runtime_library::validate_print_list(print_file.all(), library)
    });
    if let Err(error) = valid {
        controls.print_error.set(Some(format!("{error:#}")));
        return;
    }
    let image_urls = print_file
        .all()
        .iter()
        .map(FilledCardSlot::image_url)
        .collect::<Vec<_>>();
    printing.set(true);

    spawn_local(async move {
        let files_to_download = image_urls
            .iter()
            .take(69)
            .cloned()
            .chain(std::iter::once(back.clone()))
            .collect::<HashSet<_>>();
        let downloaded_files = files_to_download
            .into_iter()
            .map(|url| async move {
                let request_url = normalize_request_url(&url);
                let bytes = reqwest::get(&request_url)
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;
                let image = ImageReader::new(Cursor::new(bytes))
                    .with_guessed_format()?
                    .decode()?;
                Ok::<_, anyhow::Error>((url, image))
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<HashMap<String, DynamicImage>>>();
        let mut downloaded_files = match downloaded_files {
            Ok(files) => files,
            Err(_) => {
                controls.print_error.set(Some(
                    "A card image could not be loaded. Try again before exporting.".to_owned(),
                ));
                printing.set(false);
                return;
            }
        };
        for image in downloaded_files.values_mut() {
            *image = image.resize_exact(405, 567, image::imageops::FilterType::CatmullRom);
        }
        let print_slots = print_file.all();
        let height = (print_slots.len() as u32 + 1).div_ceil(10);
        let mut output = DynamicImage::new(4050, height * 567, image::ColorType::Rgba8);
        let mut row = 0;
        let mut column = 0;
        for (i, url) in image_urls.iter().take(69).enumerate() {
            column = i % 10;
            row = i / 10;
            let slot_image = &downloaded_files[url];
            overlay(
                &mut output,
                slot_image,
                column as i64 * 405,
                row as i64 * 567,
            );
        }
        column += 1;
        if column == 10 {
            column = 0;
            row += 1;
        }
        let back_image = &downloaded_files[&back];
        for column in column..10 {
            overlay(
                &mut output,
                back_image,
                column as i64 * 405,
                row as i64 * 567,
            );
        }

        let mut output_bytes = Cursor::new(Vec::new());
        output
            .write_to(&mut output_bytes, image::ImageFormat::Png)
            .expect("Cannot write to bytes");
        let output_bytes = output_bytes.into_inner();
        let js_bytes = Uint8Array::new_with_length(output_bytes.len() as u32);
        js_bytes.copy_from(&output_bytes);
        let js_array = JsValue::from(Box::new([js_bytes]) as Box<[_]>);
        let js_bytes_blob = Blob::new_with_buffer_source_sequence(&js_array).expect("blob");
        let link = document()
            .create_element("a")
            .expect("element")
            .dyn_into::<web_sys::HtmlAnchorElement>()
            .expect("anchor");
        let url = Url::create_object_url_with_blob(&js_bytes_blob).expect("url");
        link.set_href(&url);
        link.set_download("proxies.pdf");
        let body = document().body().expect("body");
        let cld = body.append_child(&link).expect("append");
        link.click();
        body.remove_child(&cld).expect("remove");
        Url::revoke_object_url(&url).expect("revoke");
        printing.set(false);
    });
}

fn do_nrdb_import(
    from: &str,
    import_status: Subfield<Store<AppState>, AppState, Option<ImportStatus>>,
    open_dialog: Subfield<Store<AppState>, AppState, Option<OpenDialog>>,
) {
    let (_print_file, set_print_file) = use_print_file();

    import_status.set(Some(ImportStatus::Importing));

    let public_list = Regex::new(r#"deck\/view\/([0-9a-f-]+)"#).unwrap();
    let published_list = Regex::new(r#"decklist\/([0-9a-f-]+)\/"#).unwrap();

    let public_list_id = public_list
        .captures(from)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str());
    let published_list_id = published_list
        .captures(from)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str());

    let query_url = match (public_list_id, published_list_id) {
        (Some(id), _) => format!("https://netrunnerdb.com/api/2.0/public/deck/{id}"),
        (_, Some(id)) => format!("https://netrunnerdb.com/api/2.0/public/decklist/{id}"),
        _ => {
            import_status.set(Some(ImportStatus::InvalidFormat));
            return;
        }
    };

    spawn_local(async move {
        let Ok(data) = reqwest::get(query_url).await else {
            import_status.set(Some(ImportStatus::CouldNotFind));
            return;
        };
        let Ok(data) = data.json::<serde_json::Value>().await else {
            console_error("Failed to parse JSON");
            import_status.set(Some(ImportStatus::Failed));
            return;
        };
        let Some(data) = data.get("data") else {
            console_error("JSON missing `data`");
            import_status.set(Some(ImportStatus::Failed));
            return;
        };
        let Some(data) = data.as_array() else {
            console_error("JSON `data` is not an array");
            import_status.set(Some(ImportStatus::Failed));
            return;
        };
        let Some(deck) = data.get(0) else {
            console_error("JSON `data` is empty");
            import_status.set(Some(ImportStatus::Failed));
            return;
        };
        let Some(cards) = deck.get("cards") else {
            console_error("JSON `deck` is missing `cards`");
            import_status.set(Some(ImportStatus::Failed));
            return;
        };
        let Some(cards) = cards.as_object() else {
            console_error("JSON `deck.cards` is not an object");
            import_status.set(Some(ImportStatus::Failed));
            return;
        };
        'nrdb_card: for (card, count) in cards {
            let Some(count) = count.as_u64() else {
                console_error("JSON `deck.cards` value is not a number");
                import_status.set(Some(ImportStatus::Failed));
                return;
            };
            let Ok(nrdb_printing) = card.parse::<u32>() else {
                console_error("JSON `deck.cards` key is not a number");
                import_status.set(Some(ImportStatus::Failed));
                return;
            };
            let nrdb_printing = with_library(|library| {
                library
                    .nrdb_remap
                    .get(&nrdb_printing)
                    .copied()
                    .unwrap_or(nrdb_printing)
            });
            console_log(&format!("Importing {count} {nrdb_printing}"));
            let cards = with_library(|library| {
                library.libraries["english"]
                    .cards
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
            });
            for card in cards {
                if card
                    .printings
                    .iter()
                    .any(|printing| printing.id == nrdb_printing)
                {
                    for _ in 0..count {
                        set_print_file.write().add_cards(&card);
                    }
                    continue 'nrdb_card;
                }
            }
            console_warn(&format!("Cannot find {nrdb_printing}"));
        }
        import_status.set(None);
        open_dialog.set(None);
    });
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::cast_possible_truncation)]
fn do_print(printing: Subfield<Store<AppState>, AppState, bool>, controls: PrintFeedback) {
    controls.print_error.set(None);
    let (print_file, _) = use_print_file();
    let (print_config, _) = use_print_config();
    let print_file = print_file.get_untracked();
    let valid = with_library(|library| {
        proxy_elev::runtime_library::validate_print_list(print_file.all(), library)
    });
    if let Err(error) = valid {
        controls.print_error.set(Some(format!("{error:#}")));
        return;
    }
    let image_urls = print_file
        .all()
        .iter()
        .map(FilledCardSlot::image_url)
        .collect::<Vec<_>>();
    printing.set(true);
    let print_config = print_config.get();

    spawn_local(async move {
        let mut doc = PdfDocument::new("proxies");
        let files_to_download = image_urls.iter().cloned().collect::<HashSet<_>>();
        let downloaded_files = files_to_download
            .into_iter()
            .map(|url| async move {
                let request_url = normalize_request_url(&url);
                let bytes = reqwest::get(&request_url)
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;
                let mut errs = Vec::new();
                let image = RawImage::decode_from_bytes_async(&bytes, &mut errs)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
                Ok::<_, anyhow::Error>((url, image))
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<HashMap<String, RawImage>>>();
        let downloaded_files = match downloaded_files {
            Ok(files) => files,
            Err(_) => {
                controls.print_error.set(Some(
                    "A card image could not be loaded. Try again before generating the PDF."
                        .to_owned(),
                ));
                printing.set(false);
                return;
            }
        };

        let mut page_ops: Vec<Vec<Op>> = vec![vec![]; print_file.all().len().div_ceil(9)];
        let transforms = (0..9)
            .map(|slot| {
                let (x, y, scale) = print_config.slot(slot);
                XObjectTransform {
                    translate_x: Some(Mm(x).into()),
                    translate_y: Some(Mm(y).into()),
                    scale_x: Some(scale),
                    scale_y: Some(scale),
                    dpi: Some(300.0),
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>();
        let marks = print_config
            .marks()
            .into_iter()
            .map(|(x1, x2, y1, y2)| Op::DrawPolygon {
                polygon: Polygon {
                    rings: vec![PolygonRing {
                        points: vec![
                            LinePoint {
                                p: Point::new(Mm(x1), Mm(y1)),
                                bezier: false,
                            },
                            LinePoint {
                                p: Point::new(Mm(x2), Mm(y1)),
                                bezier: false,
                            },
                            LinePoint {
                                p: Point::new(Mm(x2), Mm(y2)),
                                bezier: false,
                            },
                            LinePoint {
                                p: Point::new(Mm(x1), Mm(y2)),
                                bezier: false,
                            },
                        ],
                    }],
                    mode: PaintMode::Fill,
                    winding_order: WindingOrder::NonZero,
                },
            })
            .collect::<Vec<_>>();
        for (i, url) in image_urls.iter().enumerate() {
            let page_index = (i + 1).div_ceil(9) - 1;
            let page_slot = i % 9;
            let id = doc.add_image(&downloaded_files[url]);
            let object = Op::UseXobject {
                id,
                transform: transforms[page_slot],
            };
            page_ops[page_index].push(object);
        }
        for page in &mut page_ops {
            page.extend(marks.clone());
        }

        let (page_width, page_height) = print_config.paper();
        let pages = page_ops
            .into_iter()
            .map(|ops| PdfPage::new(Mm(page_width), Mm(page_height), ops))
            .collect();
        let pdf_bytes = doc.with_pages(pages).save(
            &PdfSaveOptions {
                image_optimization: Some(ImageOptimizationOptions {
                    quality: None,
                    max_image_size: None,
                    dither_greyscale: None,
                    convert_to_greyscale: None,
                    auto_optimize: None,
                    format: Some(ImageCompression::Flate),
                }),
                ..PdfSaveOptions::default()
            },
            &mut vec![],
        );
        let js_bytes = Uint8Array::new_with_length(pdf_bytes.len() as u32);
        js_bytes.copy_from(&pdf_bytes);
        let js_array = JsValue::from(Box::new([js_bytes]) as Box<[_]>);
        let js_bytes_blob = Blob::new_with_buffer_source_sequence(&js_array).expect("blob");
        let link = document()
            .create_element("a")
            .expect("element")
            .dyn_into::<web_sys::HtmlAnchorElement>()
            .expect("anchor");
        let url = Url::create_object_url_with_blob(&js_bytes_blob).expect("url");
        link.set_href(&url);
        link.set_download("proxies.pdf");
        let body = document().body().expect("body");
        let cld = body.append_child(&link).expect("append");
        link.click();
        body.remove_child(&cld).expect("remove");
        Url::revoke_object_url(&url).expect("revoke");
        printing.set(false);
    });
}

// bg-zinc-700
// ring-4
// bg-zinc-800
// bg-blue-800

#[derive(PartialEq, Debug, Clone)]
struct Haystack {
    haystack: Vec<String>,
    mappings: HashMap<String, HaystackEntry>,
}
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum HaystackEntry {
    Card(CardId),
    Insert(InsertId),
    InsertGroup(String),
}

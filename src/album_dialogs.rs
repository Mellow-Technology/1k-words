//! Dialogs for albums: naming one (new, or renamed), and choosing the album
//! photos are imported into.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme, IndexPath, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, Div, Entity, Focusable as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, Subscription, Task, Window, div, px,
};

use crate::library::{Album, AlbumId, album_path, tree_order};

/// Called with the name given, to do what it's for in the background. The
/// dialog closes when it's done, or shows the error and stays open.
pub type NameCallback = Rc<dyn Fn(String, &mut Window, &mut App) -> Task<anyhow::Result<()>>>;

/// Asks for an album's name: `title` names the dialog, and `confirm` its
/// button.
pub fn prompt_for_album_name(
    title: impl Into<SharedString>,
    confirm: &'static str,
    initial: &str,
    on_confirm: NameCallback,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title.into();
    let name = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("Album name")
            .default_value(initial.to_owned())
    });
    let error: Rc<RefCell<Option<SharedString>>> = Rc::default();
    let submit = {
        let name = name.clone();
        let error = error.clone();
        Rc::new(move |window: &mut Window, cx: &mut App| {
            let value = name.read(cx).value().to_string();
            let done = on_confirm(value, window, cx);
            let error = error.clone();
            window
                .spawn(cx, async move |cx| {
                    let result = done.await;
                    cx.update(|window, cx| match result {
                        Ok(()) => window.close_dialog(cx),
                        Err(message) => {
                            *error.borrow_mut() = Some(format!("{message:#}").into());
                            window.refresh();
                        }
                    })
                    .ok();
                })
                .detach();
        })
    };
    let focus = Rc::new(std::cell::Cell::new(true));
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus.replace(false) {
            name.focus_handle(cx).focus(window, cx);
        }
        let on_enter = submit.clone();
        let submit = submit.clone();
        dialog
            .title(title.clone())
            .w(px(360.))
            // Enter, even in the name field. It stays open until it's done.
            .on_ok(move |_, window, cx| {
                on_enter(window, cx);
                false
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&name))
                    .children(error_text(error.borrow().clone(), cx)),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("confirm")
                            .label(confirm)
                            .primary()
                            .on_click(move |_, window, cx| submit(window, cx)),
                    ),
            )
    });
}

/// The album an import adds its photos to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportAlbum {
    Existing(AlbumId),
    /// A new album, made for the import, in `parent` if given.
    New {
        name: String,
        parent: Option<AlbumId>,
    },
}

/// What's chosen in the dialog's album menus.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Choice {
    /// No album, or for a new album, the top level.
    None,
    Album(AlbumId),
    New,
}

#[derive(Clone)]
struct ChoiceItem {
    title: SharedString,
    choice: Choice,
}

impl SearchableListItem for ChoiceItem {
    type Value = Choice;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Choice {
        &self.choice
    }
}

/// Every album, by its place among the others, e.g. "Trips › Japan".
fn album_items(albums: &[Album]) -> Vec<ChoiceItem> {
    tree_order(albums)
        .into_iter()
        .map(|(_, album)| ChoiceItem {
            title: album_title(albums, album.id).into(),
            choice: Choice::Album(album.id),
        })
        .collect()
}

/// An album's name, after the names of the albums it's in.
pub fn album_title(albums: &[Album], id: AlbumId) -> String {
    album_path(albums, id)
        .iter()
        .map(|album| album.name.as_str())
        .collect::<Vec<_>>()
        .join(" › ")
}

struct ImportForm {
    choice: Choice,
    /// Where a new album goes.
    parent: Choice,
    name: Entity<InputState>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

/// Asks which album, if any, to add `count` photos being imported to,
/// starting with `album` chosen. `on_import` is called with the choice when
/// the import's confirmed.
pub fn prompt_for_import_album(
    albums: &[Album],
    album: Option<AlbumId>,
    on_import: Rc<dyn Fn(Option<ImportAlbum>, &mut Window, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    let existing = album_items(albums);
    let none = ChoiceItem {
        title: "No Album".into(),
        choice: Choice::None,
    };
    let new = ChoiceItem {
        title: "New Album…".into(),
        choice: Choice::New,
    };
    let choices: Vec<ChoiceItem> = [none]
        .into_iter()
        .chain(existing.iter().cloned())
        .chain([new])
        .collect();
    let parents: Vec<ChoiceItem> = [ChoiceItem {
        title: "Top Level".into(),
        choice: Choice::None,
    }]
    .into_iter()
    .chain(existing)
    .collect();

    let initial = album.map_or(Choice::None, Choice::Album);
    let initial_ix = choices
        .iter()
        .position(|item| item.choice == initial)
        .unwrap_or(0);
    let choice_select =
        cx.new(|cx| SelectState::new(choices, Some(IndexPath::new(initial_ix)), window, cx));
    // A new album goes beside the one being looked at, by default.
    let parent = album
        .and_then(|id| albums.iter().find(|a| a.id == id))
        .and_then(|album| album.parent)
        .map_or(Choice::None, Choice::Album);
    let parent_ix = parents
        .iter()
        .position(|item| item.choice == parent)
        .unwrap_or(0);
    let parent_select =
        cx.new(|cx| SelectState::new(parents, Some(IndexPath::new(parent_ix)), window, cx));
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Album name"));

    let form = Rc::new(RefCell::new(ImportForm {
        choice: initial,
        parent,
        name,
        error: None,
        _subscriptions: Vec::new(),
    }));
    let subscriptions = vec![
        {
            let form = form.clone();
            window.subscribe(
                &choice_select,
                cx,
                move |_, event: &SelectEvent<Vec<ChoiceItem>>, window, cx| {
                    let SelectEvent::Confirm(Some(choice)) = event else {
                        return;
                    };
                    let mut form = form.borrow_mut();
                    form.choice = choice.clone();
                    form.error = None;
                    if form.choice == Choice::New {
                        form.name.focus_handle(cx).focus(window, cx);
                    }
                    window.refresh();
                },
            )
        },
        {
            let form = form.clone();
            window.subscribe(
                &parent_select,
                cx,
                move |_, event: &SelectEvent<Vec<ChoiceItem>>, _, _| {
                    if let SelectEvent::Confirm(Some(parent)) = event {
                        form.borrow_mut().parent = parent.clone();
                    }
                },
            )
        },
    ];
    form.borrow_mut()._subscriptions = subscriptions;

    // Closes the dialog and imports, unless what's chosen won't do.
    let import = {
        let form = form.clone();
        Rc::new(move |window: &mut Window, cx: &mut App| {
            let chosen = form.borrow().chosen(cx);
            match chosen {
                Ok(album) => {
                    window.close_dialog(cx);
                    on_import(album, window, cx);
                }
                Err(error) => {
                    form.borrow_mut().error = Some(error);
                    window.refresh();
                }
            }
        })
    };
    window.open_dialog(cx, move |dialog, _, cx| {
        let state = form.borrow();
        let mut body = div().flex().flex_col().gap_3().child(form_row(
            "Add the photos to",
            Select::new(&choice_select).menu_max_h(px(320.)),
            cx,
        ));
        if state.choice == Choice::New {
            body = body
                .child(form_row("Name", Input::new(&state.name), cx))
                .child(form_row(
                    "Inside",
                    Select::new(&parent_select).menu_max_h(px(320.)),
                    cx,
                ));
        }
        body = body.children(error_text(state.error.clone(), cx));
        let on_enter = import.clone();
        let import = import.clone();
        dialog
            .title("Import")
            .w(px(420.))
            // Enter, even in the name field.
            .on_ok(move |_, window, cx| {
                on_enter(window, cx);
                false
            })
            .child(body)
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("start-import")
                            .label("Import")
                            .primary()
                            .on_click(move |_, window, cx| import(window, cx)),
                    ),
            )
    });
}

impl ImportForm {
    fn chosen(&self, cx: &App) -> Result<Option<ImportAlbum>, SharedString> {
        Ok(match &self.choice {
            Choice::None => None,
            Choice::Album(id) => Some(ImportAlbum::Existing(*id)),
            Choice::New => {
                let name = self.name.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    return Err("Give the new album a name.".into());
                }
                let parent = match self.parent {
                    Choice::Album(id) => Some(id),
                    _ => None,
                };
                Some(ImportAlbum::New { name, parent })
            }
        })
    }
}

fn form_row(label: &'static str, control: impl IntoElement, _cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(control)
}

fn error_text(error: Option<SharedString>, cx: &App) -> Option<Div> {
    error.map(|error| div().text_sm().text_color(cx.theme().danger).child(error))
}

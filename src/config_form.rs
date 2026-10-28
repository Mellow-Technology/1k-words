//! A form for a plugin's settings (its `ConfigField`s), as the dialogs to
//! set up a sync target or a detection plugin show it.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Div, Entity, IntoElement, ParentElement, SharedString, Styled, Window,
    div,
};

use crate::plugins::{Config, ConfigField, FieldKind};

/// A setting's input.
enum FieldInput {
    Text(Entity<InputState>),
    Flag(Rc<Cell<bool>>),
}

pub struct ConfigForm {
    fields: Vec<(ConfigField, FieldInput)>,
}

impl ConfigForm {
    /// Inputs for `fields`, filled in from `config`, or with their
    /// defaults.
    pub fn new(fields: &[ConfigField], config: &Config, window: &mut Window, cx: &mut App) -> Self {
        let fields = fields
            .iter()
            .map(|field| {
                let value = config
                    .iter()
                    .find(|(key, _)| *key == field.key)
                    .map(|(_, value)| value.clone())
                    .or_else(|| field.default.clone());
                let input = if field.kind == FieldKind::Flag {
                    FieldInput::Flag(Rc::new(Cell::new(value.as_deref() == Some("true"))))
                } else {
                    FieldInput::Text(cx.new(|cx| {
                        InputState::new(window, cx)
                            .default_value(value.unwrap_or_default())
                            .masked(field.kind == FieldKind::Secret)
                    }))
                };
                (field.clone(), input)
            })
            .collect();
        Self { fields }
    }

    /// The settings as they've been filled in.
    pub fn config(&self, cx: &App) -> Config {
        self.fields
            .iter()
            .map(|(field, input)| {
                let value = match input {
                    FieldInput::Text(state) => state.read(cx).value().to_string(),
                    FieldInput::Flag(checked) => checked.get().to_string(),
                };
                (field.key.clone(), value)
            })
            .collect()
    }

    /// Types `values` (by key) into the text fields, as a test would.
    #[cfg(test)]
    pub fn fill(&self, values: &[(&str, &str)], window: &mut Window, cx: &mut App) {
        for (field, input) in &self.fields {
            let FieldInput::Text(state) = input else {
                continue;
            };
            if let Some((_, value)) = values.iter().find(|(key, _)| *key == field.key) {
                let value = value.to_string();
                state.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        }
    }

    /// A row for each setting: its label, input and help.
    pub fn rows(&self, cx: &App) -> Vec<Div> {
        self.fields
            .iter()
            .map(|(field, input)| {
                let control = match input {
                    FieldInput::Text(state) => Input::new(state).into_any_element(),
                    FieldInput::Flag(checked) => {
                        let checked = checked.clone();
                        Checkbox::new(SharedString::from(field.key.clone()))
                            .label(field.label.clone())
                            .checked(checked.get())
                            .on_click(move |value, window, _| {
                                checked.set(*value);
                                window.refresh();
                            })
                            .into_any_element()
                    }
                };
                let label = match field.kind {
                    FieldKind::Flag => None,
                    _ if field.required => Some(format!("{} (required)", field.label)),
                    _ => Some(field.label.clone()),
                };
                form_row(
                    label.as_deref().unwrap_or(""),
                    field.help.as_deref(),
                    control,
                    cx,
                )
            })
            .collect()
    }
}

pub fn form_row(label: &str, help: Option<&str>, control: impl IntoElement, cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .when(!label.is_empty(), |row| {
            row.child(div().text_sm().child(label.to_owned()))
        })
        .child(control)
        .when_some(help.map(str::to_owned), |row, help| {
            row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(help),
            )
        })
}

use twilight_model::channel::message::{
    component::{
        ActionRow, Button, ButtonStyle, Component, SelectMenu, SelectMenuOption, SelectMenuType,
    },
    EmojiReactionType,
};

/// Wrap a row of Discord components in an `ActionRow`.
pub fn action_row(components: Vec<Component>) -> Component {
    Component::ActionRow(ActionRow { components })
}

/// Build an enabled labeled Discord button component.
pub fn button(
    custom_id: impl Into<String>,
    label: impl Into<String>,
    style: ButtonStyle,
) -> Component {
    button_disabled(custom_id, label, style, false)
}

/// Build a labeled Discord button component with an explicit `disabled` flag.
pub fn button_disabled(
    custom_id: impl Into<String>,
    label: impl Into<String>,
    style: ButtonStyle,
    disabled: bool,
) -> Component {
    Component::Button(Button {
        sku_id: None,
        custom_id: Some(custom_id.into()),
        disabled,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
    })
}

/// Build an emoji-only Discord button component.
pub fn emoji_button(
    custom_id: impl Into<String>,
    emoji: EmojiReactionType,
    style: ButtonStyle,
    disabled: bool,
) -> Component {
    Component::Button(Button {
        sku_id: None,
        custom_id: Some(custom_id.into()),
        disabled,
        emoji: Some(emoji),
        label: None,
        style,
        url: None,
    })
}

/// Build a Discord URL link button component.
pub fn link_button(label: impl Into<String>, url: impl Into<String>) -> Component {
    Component::Button(Button {
        sku_id: None,
        custom_id: None,
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style: ButtonStyle::Link,
        url: Some(url.into()),
    })
}

/// Build a single-select Discord text `SelectMenu` component.
pub fn text_select(
    custom_id: impl Into<String>,
    placeholder: impl Into<String>,
    options: Vec<SelectMenuOption>,
) -> Component {
    Component::SelectMenu(SelectMenu {
        custom_id: custom_id.into(),
        disabled: false,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(options),
        placeholder: Some(placeholder.into()),
        channel_types: None,
        default_values: None,
        kind: SelectMenuType::Text,
    })
}

/// Build a single-select Discord text `SelectMenu` wrapped in an `ActionRow`.
pub fn text_select_row(
    custom_id: impl Into<String>,
    placeholder: impl Into<String>,
    options: Vec<SelectMenuOption>,
) -> Component {
    action_row(vec![text_select(custom_id, placeholder, options)])
}

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use serenity::model::application::component::ButtonStyle;

const DEFAULT: ButtonStyle = ButtonStyle::Primary;
const DEFAULT_STR: &str = "blue";

pub fn parse_color(s: &str) -> ButtonStyle {
    match s {
        "blue" => ButtonStyle::Primary,
        "green" => ButtonStyle::Success,
        "red" => ButtonStyle::Danger,
        "grey" => ButtonStyle::Secondary,
        _ => DEFAULT,
    }
}

pub fn as_str(style: ButtonStyle) -> &'static str {
    match style {
        ButtonStyle::Primary => "blue",
        ButtonStyle::Success => "green",
        ButtonStyle::Danger => "red",
        ButtonStyle::Secondary => "grey",
        _ => DEFAULT_STR,
    }
}

pub fn determinist<T: Hash>(t: &T) -> ButtonStyle {
    let mut hasher = DefaultHasher::new();
    t.hash(&mut hasher);
    [
        ButtonStyle::Primary,
        ButtonStyle::Success,
        ButtonStyle::Danger,
        ButtonStyle::Secondary,
    ][hasher.finish() as usize % 4]
}

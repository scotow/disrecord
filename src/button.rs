use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use serenity::{
    builder::CreateButton,
    model::{application::component::ButtonStyle, channel::ReactionType},
};

use crate::soundboard::SoundMetadata;

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

pub fn determinist<T: Hash>(t: &T, with_grey: bool) -> ButtonStyle {
    let mut hasher = DefaultHasher::new();
    t.hash(&mut hasher);
    if with_grey {
        [
            ButtonStyle::Primary,
            ButtonStyle::Success,
            ButtonStyle::Danger,
            ButtonStyle::Secondary,
        ][hasher.finish() as usize % 4]
    } else {
        [
            ButtonStyle::Primary,
            ButtonStyle::Success,
            ButtonStyle::Danger,
        ][hasher.finish() as usize % 3]
    }
}

pub enum SoundButton {
    Sound(SoundMetadata),
    Random(Option<String>),
    Latest,
}

impl SoundButton {
    pub fn apply<'a>(&self, create_button: &'a mut CreateButton) -> &'a mut CreateButton {
        create_button
            .custom_id(match self {
                SoundButton::Sound(sound) => sound.id.to_string(),
                SoundButton::Random(Some(group)) => {
                    let mut hasher = DefaultHasher::new();
                    group.hash(&mut hasher);
                    format!("random-{}", hasher.finish())
                }
                SoundButton::Random(None) => "random".to_string(),
                SoundButton::Latest => "latest".to_string(),
            })
            .style(match self {
                SoundButton::Sound(sound) => sound.color,
                SoundButton::Random(_) => ButtonStyle::Primary,
                SoundButton::Latest => ButtonStyle::Success,
            })
            .label(match self {
                SoundButton::Sound(sound) => &sound.name,
                SoundButton::Random(_) => "Random",
                SoundButton::Latest => "Latest",
            });
        match self {
            SoundButton::Sound(sound) => {
                if let Some(emoji) = &sound.emoji {
                    create_button.emoji(ReactionType::Unicode(emoji.clone()));
                }
            }
            SoundButton::Random(_) => {
                create_button.emoji(ReactionType::from('🎲'));
            }
            SoundButton::Latest => {
                create_button.emoji(ReactionType::from('➡'));
            }
        }
        create_button
    }
}

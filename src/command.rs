use std::time::Duration;

use regex::Regex;
use serenity::model::{
    application::interaction::application_command::{
        ApplicationCommandInteraction, CommandDataOptionValue,
    },
    channel::Attachment,
    user::User,
};

/// Only check for a depth of 1 if `top_level` if set to false.
fn find_option<'a>(
    command: &'a ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
) -> Option<&'a CommandDataOptionValue> {
    let options = if top_level {
        &command.data.options
    } else {
        &command.data.options.first()?.options
    };
    options
        .iter()
        .find(|opt| opt.name == name)
        .and_then(|opt| opt.resolved.as_ref())
}

pub fn find_string_option<'a>(
    command: &'a ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
    default: Option<&'a str>,
) -> Option<&'a str> {
    match find_option(command, name, top_level) {
        Some(CommandDataOptionValue::String(s)) => Some(s),
        Some(_) => None,
        None => default,
    }
}

pub fn find_integer_option(
    command: &ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
    default: Option<i64>,
) -> Option<i64> {
    match find_option(command, name, top_level) {
        Some(CommandDataOptionValue::Integer(n)) => Some(*n),
        Some(_) => None,
        None => default,
    }
}

pub fn find_user_option<'a>(
    command: &'a ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
) -> Option<&'a User> {
    match find_option(command, name, top_level) {
        Some(CommandDataOptionValue::User(u, _)) => Some(u),
        _ => None,
    }
}

pub fn find_attachment_option<'a>(
    command: &'a ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
) -> Option<&'a Attachment> {
    match find_option(command, name, top_level) {
        Some(CommandDataOptionValue::Attachment(a)) => Some(a),
        _ => None,
    }
}

pub fn find_emoji_option(
    command: &ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
) -> Option<String> {
    let option = find_string_option(command, name, top_level, None)?;
    if emojis::get(option).is_some() {
        Some(option.to_owned())
    } else {
        Regex::new(r#"\p{Emoji}"#)
            .expect("Invalid emoji regex")
            .find(option)
            .map(|m| m.as_str().to_owned())
    }
}

pub fn find_duration_option(
    command: &ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
    default: Option<Duration>,
) -> Option<Duration> {
    match find_string_option(command, name, top_level, None) {
        Some(s) => parse_duration::parse(s).ok(),
        None => default,
    }
}

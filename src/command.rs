use std::time::Duration;

use regex::Regex;
use serenity::{
    all::{ResolvedOption, ResolvedValue},
    model::{application::CommandInteraction, channel::Attachment, user::User},
};

/// Only check for a depth of 1 if `top_level` if set to false.
fn find_option<'a>(command: &'a CommandInteraction, name: &str) -> Option<ResolvedValue<'a>> {
    fn browse<'a>(options: Vec<ResolvedOption<'a>>, name: &str) -> Option<ResolvedValue<'a>> {
        for option in options {
            if option.name == name {
                return Some(option.value);
            }
            match option.value {
                ResolvedValue::SubCommand(options) => return browse(options, name),
                _ => (),
            }
        }
        None
    }
    browse(command.data.options(), name)

    // dbg!(&command.data.options, &command.data.resolved);
    // command
    //     .data
    //     .options()
    //     .into_iter()
    //     .find(|opt| opt.name == name)
    //     .map(|opt| opt.value)

    // let options = if top_level {
    //     &command.data.options
    // } else {
    //     &command.data.options.first()?.options
    // };
    // options
    //     .iter()
    //     .find(|opt| opt.name == name)
    //     .and_then(|opt| opt.resolved.as_ref())
}

pub fn find_string_option<'a>(
    command: &'a CommandInteraction,
    name: &str,
    default: Option<&'a str>,
) -> Option<&'a str> {
    match find_option(command, name) {
        Some(ResolvedValue::String(s)) => Some(s),
        Some(_) => None,
        None => default,
    }
}

pub fn find_integer_option(
    command: &CommandInteraction,
    name: &str,
    default: Option<i64>,
) -> Option<i64> {
    match find_option(command, name) {
        Some(ResolvedValue::Integer(n)) => Some(n),
        Some(_) => None,
        None => default,
    }
}

pub fn find_boolean_option(
    command: &CommandInteraction,
    name: &str,
    default: Option<bool>,
) -> Option<bool> {
    match find_option(command, name) {
        Some(ResolvedValue::Boolean(b)) => Some(b),
        Some(_) => None,
        None => default,
    }
}

pub fn find_user_option<'a>(command: &'a CommandInteraction, name: &str) -> Option<&'a User> {
    match find_option(command, name) {
        Some(ResolvedValue::User(u, _)) => Some(u),
        _ => None,
    }
}

pub fn find_attachment_option<'a>(
    command: &'a CommandInteraction,
    name: &str,
) -> Option<&'a Attachment> {
    match find_option(command, name) {
        Some(ResolvedValue::Attachment(a)) => Some(a),
        _ => None,
    }
}

pub fn find_emoji_option(command: &CommandInteraction, name: &str) -> Option<String> {
    let option = find_string_option(command, name, None)?;
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
    command: &CommandInteraction,
    name: &str,
    default: Option<Duration>,
) -> Option<Duration> {
    match find_string_option(command, name, None) {
        Some(s) => parse_duration::parse(s).ok(),
        None => default,
    }
}

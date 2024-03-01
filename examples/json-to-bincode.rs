use std::{env, fs, fs::File, io::Write, path::PathBuf};

use clap::Parser;
use serde::{Deserialize, Serialize};
use serenity::all::ButtonStyle;
use ulid::Ulid;

fn main() {
    let options = Options::parse();
    let data = serde_json::from_slice::<Vec<JsonSoundGroup>>(
        &fs::read(options.json_path).expect("failed to read json file"),
    )
    .expect("failed to parse json");

    let mut out = File::create(options.bincode_path).expect("failed to open file for write");
    for group in data {
        for (i, sound) in group.sounds.into_iter().enumerate() {
            out.write(
                &bincode::serialize(&BincodeMetadata {
                    guild: options.guild,
                    id: sound.id,
                    name: sound.name,
                    emoji: sound.emoji,
                    color: parse_color(&sound.color),
                    group: &group.group,
                    index: i,
                })
                .expect("failed to serialize sound metadata"),
            )
            .expect("failed to write sound metadata");
        }
    }
}

#[derive(Parser)]
struct Options {
    #[clap(short, long)]
    guild: u64,
    #[clap(short, long)]
    json_path: PathBuf,
    #[clap(short, long)]
    bincode_path: PathBuf,
}

#[derive(Deserialize, Debug)]
struct JsonSoundGroup {
    group: String,
    sounds: Vec<JsonSound>,
}

#[derive(Deserialize, Debug)]
struct JsonSound {
    id: Ulid,
    name: String,
    emoji: Option<String>,
    color: String,
}

#[derive(Serialize, Clone, Debug)]
struct BincodeMetadata<'a> {
    guild: u64,
    id: Ulid,
    name: String,
    emoji: Option<String>,
    color: ButtonStyle,
    group: &'a str,
    index: usize,
}

fn parse_color(s: &str) -> ButtonStyle {
    match s {
        "blue" => ButtonStyle::Primary,
        "green" => ButtonStyle::Success,
        "red" => ButtonStyle::Danger,
        "grey" => ButtonStyle::Secondary,
        _ => unreachable!(),
    }
}

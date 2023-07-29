use std::{fs, path::PathBuf};

use bincode::Options as _;
use clap::Parser;
use serde::{Deserialize, Serialize};
use serenity::model::application::component::ButtonStyle;
use ulid::Ulid;

#[derive(Deserialize)]
pub struct SoundMetadataOld {
    guild: u64,
    pub id: Ulid,
    pub name: String,
    pub emoji: Option<char>,
    pub color: ButtonStyle,
    group: String,
    index: usize,
}

#[derive(Serialize)]
pub struct SoundMetadataNew {
    guild: u64,
    pub id: Ulid,
    pub name: String,
    pub emoji: Option<String>,
    pub color: ButtonStyle,
    group: String,
    index: usize,
}

impl From<SoundMetadataOld> for SoundMetadataNew {
    fn from(value: SoundMetadataOld) -> Self {
        Self {
            guild: value.guild,
            id: value.id,
            name: value.name,
            emoji: value.emoji.map(|e| e.to_string()),
            color: value.color,
            group: value.group,
            index: value.index,
        }
    }
}

#[derive(Parser)]
pub struct Options {
    #[arg(short, long)]
    input: PathBuf,
    #[arg(short, long)]
    output: PathBuf,
}

fn main() {
    let options = Options::parse();

    let in_data = fs::read(&options.input).expect("Failed to read input file");
    let mut deserializer = bincode::Deserializer::from_slice(
        &in_data,
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .allow_trailing_bytes(),
    );
    let mut sounds = Vec::new();
    loop {
        let metadata = match SoundMetadataOld::deserialize(&mut deserializer) {
            Ok(metadata) => metadata,
            Err(_) => break,
        };
        sounds.push(metadata);
    }

    let mut out_data = Vec::new();
    for sound in sounds {
        out_data.extend(
            bincode::serialize(&SoundMetadataNew::from(sound)).expect("Failed to serialize"),
        );
    }

    fs::write(&options.output, &out_data).expect("Failed to write data");
}

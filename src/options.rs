use std::{path::PathBuf, time::Duration};

use clap::Parser;
use parse_duration::parse::Error as DurationError;

#[derive(Parser)]
pub struct Options {
    #[arg(short = 'd', long)]
    pub discord_token: String,
    #[arg(short = 'w', long)]
    pub whitelist_path: PathBuf,
    #[arg(short = 'v', long, value_parser(Options::parse_duration))]
    pub voice_buffer_duration: Duration,
    #[arg(short = 'e', long, value_parser(Options::parse_duration))]
    pub voice_buffer_expiration: Duration,
}

impl Options {
    fn parse_duration(input: &str) -> Result<Duration, DurationError> {
        parse_duration::parse(input)
    }
}

use std::{path::PathBuf, time::Duration};

use clap::{ArgAction, Parser};
use log::LevelFilter;
use parse_duration::parse::Error as DurationError;

#[derive(Parser)]
pub struct Options {
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    log_level: u8,
    #[arg(short = 't', long)]
    pub discord_token: String,
    #[arg(short = 'w', long)]
    pub whitelist_path: PathBuf,
    #[arg(short = 'd', long, value_parser(Options::parse_duration))]
    pub voice_buffer_duration: Duration,
    #[arg(short = 'e', long, value_parser(Options::parse_duration))]
    pub voice_buffer_expiration: Duration,
}

impl Options {
    fn parse_duration(input: &str) -> Result<Duration, DurationError> {
        parse_duration::parse(input)
    }

    pub fn log_level(&self) -> LevelFilter {
        match self.log_level {
            0 => LevelFilter::Error,
            1 => LevelFilter::Warn,
            2 => LevelFilter::Info,
            3 => LevelFilter::Debug,
            4.. => LevelFilter::Trace,
        }
    }
}

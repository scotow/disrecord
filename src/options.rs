use std::{path::PathBuf, time::Duration};

use clap::{ArgAction, Parser};
use log::LevelFilter;
use parse_duration::parse::Error as DurationError;

#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Options {
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    log_level: u8,
    #[arg(short = 't', long)]
    pub discord_token: String,
    #[arg(short = 'w', long, default_value("record-whitelist"))]
    pub record_whitelist_path: PathBuf,
    #[arg(
        short = 'd',
        long,
        value_parser(Options::parse_duration),
        default_value("3m")
    )]
    pub voice_buffer_duration: Duration,
    #[arg(
        short = 'e',
        long,
        value_parser(Options::parse_duration),
        default_value("5m")
    )]
    pub voice_buffer_expiration: Duration,
    #[arg(short = 's', long, default_value("soundboard"))]
    pub soundboard_metadata_path: PathBuf,
    #[arg(short = 'S', long, default_value("."))]
    pub sounds_dir_path: PathBuf,
    #[arg(
        short = 'D',
        long,
        value_parser(Options::parse_duration),
        default_value("15s")
    )]
    pub sound_max_duration: Duration,
    #[arg(
        short = 'c',
        long,
        value_parser(Options::parse_duration),
        default_value("3m")
    )]
    pub sound_cache_duration: Duration,
    #[arg(short = 'f', long, default_value("ffmpeg"))]
    pub ffmpeg_path: PathBuf,
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

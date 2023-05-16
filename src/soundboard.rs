use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use itertools::Itertools;
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use serenity::model::{application::component::ButtonStyle, id::GuildId};
use thiserror::Error as ThisError;
use tokio::{fs, fs::OpenOptions, io::AsyncWriteExt, sync::Mutex};
use ulid::Ulid;

use crate::wav::wav_duration_from_size;

#[derive(Debug)]
pub struct Soundboard {
    metadata_path: PathBuf,
    sounds_dir_path: PathBuf,
    max_duration: Duration,
    cache_duration: Duration,
    sounds: Mutex<HashMap<Ulid, Sound>>,
}

impl Soundboard {
    pub async fn new(
        metadata_path: PathBuf,
        sounds_dir_path: PathBuf,
        max_duration: Duration,
        cache_duration: Duration,
    ) -> Self {
        let sounds = fs::read(&metadata_path)
            .await
            .ok()
            .map(|mut file| {
                let mut sounds = HashMap::new();
                let mut data = file.as_slice();
                while !data.is_empty() {
                    let metadata = bincode::deserialize::<SoundMetadata>(&data)
                        .expect("Failed to deserialize sound metadata");
                    data = &data[bincode::serialized_size(&metadata).unwrap() as usize..];
                    sounds.insert(
                        metadata.id,
                        Sound {
                            metadata,
                            data: CachedSound::Fs,
                        },
                    );
                }
                sounds
            })
            .unwrap_or_default();
        dbg!(&sounds);

        Self {
            metadata_path,
            sounds_dir_path,
            max_duration,
            cache_duration,
            sounds: Mutex::new(sounds),
        }
    }

    pub async fn list(&self, guild: GuildId) -> Vec<(String, Vec<SoundMetadata>)> {
        self.sounds
            .lock()
            .await
            .values()
            .filter(|sound| sound.metadata.guild == guild.0)
            .into_group_map_by(|sound| &sound.metadata.group)
            .into_iter()
            .sorted_by(|(g1, _), (g2, _)| g1.cmp(g2))
            .map(|(g, s)| {
                let mut sounds = s.into_iter().map(|s| s.metadata.clone()).collect_vec();
                sounds
                    .sort_by(|s1, s2| s1.index.cmp(&s2.index).then_with(|| s1.name.cmp(&s2.name)));
                (g.clone(), sounds)
            })
            .collect()
    }

    pub async fn get_wav(&self, id: Ulid) -> Option<Vec<u8>> {
        self.sounds
            .lock()
            .await
            .get_mut(&id)?
            .get_wav_data(&self.sounds_dir_path)
            .await
    }

    // TODO: Pass Attachment struct to gain size info and download helper func.
    pub async fn add(
        &self,
        attachment_url: &str,
        guild: GuildId,
        name: String,
        emoji: Option<char>,
        color: ButtonStyle,
        group: String,
        index: Option<usize>,
    ) -> Result<Ulid, SoundboardError> {
        let mut client = Client::new();

        // Fetch and verify sound duration.
        let head_response = client
            .head(attachment_url)
            .send()
            .await
            .map_err(|_| SoundboardError::SoundFetch)?;
        let size = head_response
            .headers()
            .get(header::CONTENT_LENGTH)
            .ok_or(SoundboardError::SoundFetch)?
            .to_str()
            .map_err(|_| SoundboardError::SoundFetch)?
            .parse::<usize>()
            .map_err(|_| SoundboardError::SoundFetch)?;
        if wav_duration_from_size(size) > self.max_duration {
            return Err(SoundboardError::TooLong);
        }

        // Fetch sound data.
        let data = client
            .get(attachment_url)
            .send()
            .await
            .map_err(|_| SoundboardError::SoundFetch)?
            .bytes()
            .await
            .map_err(|_| SoundboardError::SoundFetch)?
            .to_vec();

        let mut sounds = self.sounds.lock().await;
        if sounds
            .values()
            .any(|sound| sound.metadata.guild == guild.0 && sound.metadata.name == name)
        {
            return Err(SoundboardError::NameTaken);
        }

        let id = Ulid::new();
        let metadata = SoundMetadata {
            guild: guild.0,
            id,
            name,
            emoji,
            color,
            group,
            // TODO: Check index and insert last if missing.
            index: index.unwrap_or(42),
        };

        // Write sound to disk.
        fs::write(metadata.get_file_path(&self.sounds_dir_path), &data)
            .await
            .map_err(|_| SoundboardError::SoundWrite)?;
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.metadata_path)
            .await
            .map_err(|_| SoundboardError::SoundWrite)?;
        file.write(&bincode::serialize(&metadata).map_err(|_| SoundboardError::SoundWrite)?)
            .await
            .map_err(|_| SoundboardError::SoundWrite)?;

        sounds.insert(
            metadata.id,
            Sound {
                metadata,
                data: CachedSound::Cached(data, Instant::now()),
            },
        );

        Ok(id)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SoundMetadata {
    guild: u64,
    pub id: Ulid,
    pub name: String,
    pub emoji: Option<char>,
    pub color: ButtonStyle,
    group: String,
    index: usize,
}

impl SoundMetadata {
    fn get_file_path(&self, fs: &Path) -> PathBuf {
        let mut path = fs.join(self.id.to_string());
        path.set_extension("wav");
        path
    }
}

#[derive(Debug)]
struct Sound {
    metadata: SoundMetadata,
    data: CachedSound,
}

#[derive(Debug)]
enum CachedSound {
    Fs,
    Cached(Vec<u8>, Instant),
}

impl Sound {
    async fn get_wav_data(&mut self, dir_path: &Path) -> Option<Vec<u8>> {
        match &mut self.data {
            CachedSound::Fs => {
                let mut path = self.metadata.get_file_path(dir_path);
                let data = fs::read(&path).await.ok()?;
                self.data = CachedSound::Cached(data.clone(), Instant::now());
                Some(data)
            }
            CachedSound::Cached(data, fetched) => {
                *fetched = Instant::now();
                Some(data.clone())
            }
        }
    }
}

#[derive(ThisError, Debug)]
pub enum SoundboardError {
    #[error("A sound with the same name already exists.")]
    NameTaken,
    #[error("Sound too long.")]
    TooLong,
    #[error("Failed to fetch sound from Discord server.")]
    SoundFetch,
    #[error("Failed to save file.")]
    SoundWrite,
}

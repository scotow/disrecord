use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use bincode::Options;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serenity::model::{application::component::ButtonStyle, channel::Attachment, id::GuildId};
use thiserror::Error as ThisError;
use tokio::{fs, fs::OpenOptions, io::AsyncWriteExt, sync::Mutex, time::sleep};
use ulid::Ulid;

use crate::wav;

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
            .map(|file| {
                let mut deserializer = bincode::Deserializer::from_slice(
                    &file,
                    bincode::DefaultOptions::new()
                        .with_fixint_encoding()
                        .allow_trailing_bytes(),
                );
                let mut sounds = HashMap::new();
                loop {
                    let metadata = match SoundMetadata::deserialize(&mut deserializer) {
                        Ok(metadata) => metadata,
                        Err(_) => break,
                    };
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

        Self {
            metadata_path,
            sounds_dir_path,
            max_duration,
            cache_duration,
            sounds: Mutex::new(sounds),
        }
    }

    pub fn cache_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(30)).await;
                for sound in self.sounds.lock().await.values_mut() {
                    if let CachedSound::Cached(_, last) = &mut sound.data {
                        if last.elapsed() > self.cache_duration {
                            sound.data = CachedSound::Fs;
                        }
                    }
                }
            }
        });
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

    #[allow(clippy::too_many_arguments)]
    pub async fn add(
        &self,
        attachment: &Attachment,
        guild: GuildId,
        name: String,
        emoji: Option<char>,
        color: ButtonStyle,
        group: String,
        index: Option<usize>,
    ) -> Result<Ulid, SoundboardError> {
        // Verify duration.
        if wav::duration_from_size(attachment.size as usize) > self.max_duration {
            return Err(SoundboardError::TooLong);
        }

        // Fetch sound data.
        let data = attachment
            .download()
            .await
            .map_err(|_| SoundboardError::SoundFetch)?;

        // Verify sound format.
        if !wav::is_valid(&data) {
            return Err(SoundboardError::InvalidSound);
        }

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
                let path = self.metadata.get_file_path(dir_path);
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
    #[error("Sound file is not of the right format/encoding.")]
    InvalidSound,
    #[error("Failed to save file.")]
    SoundWrite,
}

use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use bincode::Options;
use itertools::Itertools;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use serenity::model::{application::component::ButtonStyle, channel::Attachment, id::GuildId};
use thiserror::Error as ThisError;
use tokio::{fs, fs::OpenOptions, io::AsyncWriteExt, process::Command, sync::Mutex, time::sleep};
use ulid::Ulid;

use crate::wav;

#[derive(Debug)]
pub struct Soundboard {
    metadata_path: PathBuf,
    sounds_dir_path: PathBuf,
    max_duration: Duration,
    cache_duration: Duration,
    ffmpeg_path: PathBuf,
    sounds: Mutex<HashMap<Ulid, Sound>>,
}

impl Soundboard {
    pub async fn new(
        metadata_path: PathBuf,
        sounds_dir_path: PathBuf,
        max_duration: Duration,
        cache_duration: Duration,
        ffmpeg_path: PathBuf,
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
            ffmpeg_path,
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

    pub async fn names_matching(&self, guild: GuildId, search: &str, max: usize) -> Vec<String> {
        let regex = search_regex(search);
        self.sounds
            .lock()
            .await
            .values()
            .filter_map(|sound| {
                (sound.metadata.guild == guild.0 && regex.is_match(&sound.metadata.name))
                    .then(|| sound.metadata.name.clone())
            })
            .take(max)
            .collect()
    }

    pub async fn groups_matching(&self, guild: GuildId, group: &str, max: usize) -> Vec<String> {
        let regex = search_regex(group);
        self.sounds
            .lock()
            .await
            .values()
            .filter_map(|sound| {
                (sound.metadata.guild == guild.0 && regex.is_match(&sound.metadata.group))
                    .then(|| sound.metadata.group.clone())
            })
            .unique()
            .take(max)
            .collect()
    }

    pub async fn get_wav(&self, id: Ulid) -> Option<Vec<u8>> {
        self.sounds
            .lock()
            .await
            .get_mut(&id)?
            .get_wav_data(&self.sounds_dir_path, true)
            .await
    }

    pub async fn get_wav_by_name(&self, name: &str, guild: GuildId) -> Option<Vec<u8>> {
        let regex = match_regex(name);
        self.sounds
            .lock()
            .await
            .values_mut()
            .find(|sound| sound.metadata.guild == guild.0 && regex.is_match(&sound.metadata.name))?
            .get_wav_data(&self.sounds_dir_path, true)
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
        requested_index: Option<usize>,
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

        // Verify sound format and transcode with ffmpeg if needed.
        let data = if wav::is_valid(&data) {
            data
        } else {
            let filename = PathBuf::from(&attachment.filename);
            let extension = filename
                .extension()
                .and_then(OsStr::to_str)
                .ok_or(SoundboardError::InvalidSound)?;

            let mut cmd = Command::new(&self.ffmpeg_path);
            cmd.args(["-f", extension]) // Input file format.
                .args(["-i", "-"]) // Read from stdin.
                .args(["-c:a", "pcm_s16le"]) // Transcode to PCM i16 LE.
                .args(["-f", "s16le"]) // Output to raw PCM.
                .args(["-ac", "1"]) // Mono channel.
                .args(["-ar", "48000"]) // 48kHz sample rate.
                .arg("-") // Output to stdout.
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = cmd
                .spawn()
                .map_err(|_| SoundboardError::TranscodingFailed)?;

            let mut stdin = child
                .stdin
                .take()
                .ok_or(SoundboardError::TranscodingFailed)?;
            tokio::spawn(async move {
                stdin
                    .write_all(&data)
                    .await
                    .expect("Failed to write sound to ffmpeg");
            });

            let out = child
                .wait_with_output()
                .await
                .map_err(|_| SoundboardError::TranscodingFailed)?;
            if !out.status.success() || out.stdout.len() % 2 != 0 {
                return Err(SoundboardError::TranscodingFailed);
            }

            let mut data = out.stdout;
            wav::package_mut_raw(&mut data);
            data
        };

        let regex = match_regex(&name);
        let mut sounds = self.sounds.lock().await;
        if sounds
            .values()
            .any(|sound| sound.metadata.guild == guild.0 && regex.is_match(&sound.metadata.name))
        {
            return Err(SoundboardError::NameTaken);
        }

        // Resolve index position.
        let mut overwrite_required = false;
        let group_indexes = sounds.values().filter_map(|s| {
            (s.metadata.guild == guild.0 && s.metadata.group == group).then_some(s.metadata.index)
        });
        let last_index = group_indexes.clone().max();

        // IDEA: Append with gap of 1M. And when inserting between, insert at equal
        // distance between (N-1) and (N+1).
        let index = match (last_index, requested_index) {
            (None, _) => 0,
            (Some(last_index), None) => last_index + 1,
            (Some(last_index), Some(requested_index)) if requested_index > last_index => {
                last_index + 1
            }
            (Some(_), Some(requested_index)) => {
                // If there is a gap in the indexes, use it.
                if !group_indexes.clone().contains(&requested_index) {
                    requested_index
                } else {
                    overwrite_required = true;
                    for sound_index in sounds.values_mut().filter_map(|s| {
                        (s.metadata.guild == guild.0
                            && s.metadata.group == group
                            && s.metadata.index >= requested_index)
                            .then_some(&mut s.metadata.index)
                    }) {
                        *sound_index += 1;
                    }
                    requested_index
                }
            }
        };

        let id = Ulid::new();
        let metadata = SoundMetadata {
            guild: guild.0,
            id,
            name,
            emoji,
            color,
            group,
            index,
        };

        // Write sound to disk.
        fs::write(metadata.get_file_path(&self.sounds_dir_path), &data)
            .await
            .map_err(|_| SoundboardError::SoundWrite)?;
        if overwrite_required {
            self.overwrite_metadata_file(&sounds).await?;
        } else {
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.metadata_path)
                .await
                .map_err(|_| SoundboardError::SoundWrite)?;
            file.write(&bincode::serialize(&metadata).map_err(|_| SoundboardError::SoundWrite)?)
                .await
                .map_err(|_| SoundboardError::SoundWrite)?;
        }

        sounds.insert(
            metadata.id,
            Sound {
                metadata,
                data: CachedSound::Cached(data, Instant::now()),
            },
        );

        Ok(id)
    }

    pub async fn delete(&self, guild: GuildId, name: &str) -> Result<(), SoundboardError> {
        let regex = match_regex(name);
        let mut sounds = self.sounds.lock().await;
        let id = sounds
            .iter()
            .find_map(|(id, sound)| {
                (sound.metadata.guild == guild.0 && regex.is_match(&sound.metadata.name))
                    .then_some(*id)
            })
            .ok_or(SoundboardError::SoundNotFound)?;

        let sound = sounds.remove(&id).ok_or(SoundboardError::SoundNotFound)?;
        self.overwrite_metadata_file(&sounds).await?;
        fs::remove_file(sound.metadata.get_file_path(&self.sounds_dir_path))
            .await
            .map_err(|_| SoundboardError::DeleteFailed)
    }

    async fn overwrite_metadata_file(
        &self,
        sounds: &HashMap<Ulid, Sound>,
    ) -> Result<(), SoundboardError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.metadata_path)
            .await
            .map_err(|_| SoundboardError::SoundWrite)?;

        for sound in sounds.values() {
            file.write(
                &bincode::serialize(&sound.metadata).map_err(|_| SoundboardError::SoundWrite)?,
            )
            .await
            .map_err(|_| SoundboardError::SoundWrite)?;
        }

        Ok(())
    }

    pub async fn backup(
        &self,
        guild: GuildId,
    ) -> Result<(String, Vec<(String, Vec<u8>)>), SoundboardError> {
        let mut sounds = self.sounds.lock().await;

        let metadata = sounds
            .values()
            .filter(|sound| sound.metadata.guild == guild.0)
            .clone()
            .into_group_map_by(|sound| &sound.metadata.group)
            .into_iter()
            .sorted_by(|(g1, _), (g2, _)| g1.cmp(g2))
            .map(|(group, mut sounds)| {
                sounds.sort_by(|s1, s2| {
                    s1.metadata
                        .index
                        .cmp(&s2.metadata.index)
                        .then_with(|| s1.metadata.name.cmp(&s2.metadata.name))
                });
                let sounds = sounds
                    .into_iter()
                    .map(|sound| {
                        json!({
                            "id": sound.metadata.id.to_string(),
                            "name": sound.metadata.name,
                            "emoji": sound.metadata.emoji,
                            "color": match sound.metadata.color {
                                ButtonStyle::Primary => "blue",
                                ButtonStyle::Success => "green",
                                ButtonStyle::Danger => "red",
                                ButtonStyle::Secondary => "grey",
                                _ => "blue",
                            }
                        })
                    })
                    .collect::<Value>();
                json!({
                    "group": group,
                    "sounds": sounds,
                })
            })
            .collect::<Value>();

        let mut data = Vec::new();
        for sound in sounds.values_mut() {
            data.push((
                format!("{}.wav", sound.metadata.id.to_string()),
                sound
                    .get_wav_data(&self.sounds_dir_path, false)
                    .await
                    .ok_or(SoundboardError::BackupFailed)?,
            ));
        }

        Ok((
            serde_json::to_string_pretty(&metadata).map_err(|_| SoundboardError::BackupFailed)?,
            data,
        ))
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
    fn get_file_path(&self, dir_path: &Path) -> PathBuf {
        let mut path = dir_path.join(self.id.to_string());
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
    async fn get_wav_data(&mut self, dir_path: &Path, cache: bool) -> Option<Vec<u8>> {
        match &mut self.data {
            CachedSound::Fs => {
                let path = self.metadata.get_file_path(dir_path);
                let data = fs::read(&path).await.ok()?;
                if cache {
                    self.data = CachedSound::Cached(data.clone(), Instant::now());
                }
                Some(data)
            }
            CachedSound::Cached(data, fetched) => {
                if cache {
                    *fetched = Instant::now();
                }
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
    #[error("Failed to transcode sound to supported format.")]
    TranscodingFailed,
    #[error("Failed to save file.")]
    SoundWrite,
    #[error("Cannot find that sound.")]
    SoundNotFound,
    #[error("Failed to delete sound.")]
    DeleteFailed,
    #[error("Failed to create backup.")]
    BackupFailed,
}

fn match_regex(searching: &str) -> Regex {
    Regex::new(&format!("(?i){}", regex::escape(searching))).expect("Failed to build match regex")
}

fn search_regex(searching: &str) -> Regex {
    Regex::new(&format!("(?i).*{}.*", regex::escape(searching)))
        .expect("Failed to build search regex")
}

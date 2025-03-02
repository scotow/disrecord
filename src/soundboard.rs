use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    ffi::OsStr,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use bincode::Options;
use itertools::Itertools;
use log::info;
use rand::seq::IteratorRandom;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::{
    all::ButtonStyle,
    model::{channel::Attachment, id::GuildId},
};
use thiserror::Error as ThisError;
use tokio::{fs, fs::OpenOptions, io::AsyncWriteExt, process::Command, sync::Mutex, time::sleep};
use ulid::Ulid;

use crate::{button, wav};

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
        info!("loaded {} sounds", sounds.len());

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
            .filter(|sound| sound.metadata.guild == guild.get())
            .into_group_map_by(|sound| &sound.metadata.group)
            .into_iter()
            .sorted_by(|(g1, _), (g2, _)| g1.cmp(g2))
            .map(|(g, s)| {
                let mut sounds = s.into_iter().map(|s| s.metadata.clone()).collect_vec();
                sounds.sort_by_key(|sound| sound.index);
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
                (sound.metadata.guild == guild.get() && regex.is_match(&sound.metadata.name))
                    .then(|| sound.metadata.name.clone())
            })
            .sorted()
            .dedup()
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
                (sound.metadata.guild == guild.get() && regex.is_match(&sound.metadata.group))
                    .then(|| sound.metadata.group.clone())
            })
            .sorted()
            .dedup()
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

    pub async fn get_wav_by_name(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
    ) -> Result<Vec<u8>, SoundboardError> {
        let name_regex = match_regex(name);
        let group_regex = group.map(match_regex);

        let mut sounds = self.sounds.lock().await;
        let mut matching = sounds.values_mut().filter(|sound| {
            sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && group_regex
                    .as_ref()
                    .map(|rg| rg.is_match(&sound.metadata.group))
                    .unwrap_or(true)
        });
        let sound = matching.next().ok_or(SoundboardError::SoundNotFound)?;
        if matching.next().is_some() {
            return Err(SoundboardError::SoundNameAmbiguous);
        }

        sound
            .get_wav_data(&self.sounds_dir_path, true)
            .await
            .ok_or(SoundboardError::SoundNotFound)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add(
        &self,
        attachment: &Attachment,
        guild: GuildId,
        name: String,
        emoji: Option<String>,
        color: ButtonStyle,
        mut group: String,
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

        // If sound is already PCM s16le WAV, keep it as is, transcode it otherwise.
        let data = if wav::is_valid_pcm_s16le(&data) {
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
                .args(["-f", "wav"]) // Output to raw PCM.
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

            out.stdout
        };

        let mut sounds = self.sounds.lock().await;

        // Find similar existing group.
        let group_regex = match_regex(&group);
        group = sounds
            .values()
            .find_map(|sound| {
                group_regex
                    .is_match(&sound.metadata.group)
                    .then(|| sound.metadata.group.clone())
            })
            .unwrap_or(group);

        // Check if name is already taken in this group.
        let name_regex = match_regex(&name);
        if sounds.values().any(|sound| {
            sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && sound.metadata.group == group
        }) {
            return Err(SoundboardError::NameTaken);
        }

        // Resolve index position.
        let mut overwrite_required = false;
        let group_sounds = sounds
            .values()
            .filter(|s| s.metadata.guild == guild.get() && s.metadata.group == group);
        let last_index = group_sounds.clone().map(|s| s.metadata.index).max();

        // IDEA: Append with gap of 1M. And when inserting between, insert at equal
        // distance between (N-1) and (N+1).
        let index = match (last_index, requested_index) {
            (None, _) => 0,
            (Some(last_index), None) => last_index + 1,
            (Some(last_index), Some(requested_index)) => {
                let group_len = group_sounds.clone().count();
                if requested_index >= group_len {
                    last_index + 1
                } else {
                    overwrite_required = true;
                    let mut sounds = sounds
                        .values_mut()
                        .filter(|s| s.metadata.guild == guild.get() && s.metadata.group == group)
                        .collect_vec();
                    sounds.sort_by_key(|s| s.metadata.index);
                    let prev_index = sounds[requested_index].metadata.index;
                    for sound in &mut sounds[requested_index..] {
                        sound.metadata.index += 1;
                    }
                    prev_index
                }
            }
        };

        let id = Ulid::new();
        let metadata = SoundMetadata {
            guild: guild.get(),
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

        // Write metadata to disk (partial or full overwrite).
        sounds.insert(
            metadata.id,
            Sound {
                metadata: metadata.clone(),
                data: CachedSound::Cached(data, Instant::now()),
            },
        );
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

        Ok(id)
    }

    pub async fn delete(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
    ) -> Result<(), SoundboardError> {
        let name_regex = match_regex(name);
        let group_regex = group.map(match_regex);

        let mut sounds = self.sounds.lock().await;
        let mut matching = sounds.iter().filter_map(|(id, sound)| {
            (sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && group_regex
                    .as_ref()
                    .map(|rg| rg.is_match(&sound.metadata.group))
                    .unwrap_or(true))
            .then_some(*id)
        });
        let id = matching.next().ok_or(SoundboardError::SoundNotFound)?;
        if matching.next().is_some() {
            return Err(SoundboardError::SoundNameAmbiguous);
        }

        let sound = sounds.remove(&id).ok_or(SoundboardError::SoundNotFound)?;
        self.overwrite_metadata_file(&sounds).await?;
        fs::remove_file(sound.metadata.get_file_path(&self.sounds_dir_path))
            .await
            .map_err(|_| SoundboardError::DeleteFailed)
    }

    pub async fn delete_id(&self, guild: GuildId, id: Ulid) -> Result<(), SoundboardError> {
        let mut sounds = self.sounds.lock().await;
        let sound = sounds.remove(&id).ok_or(SoundboardError::SoundNotFound)?;
        assert_eq!(sound.metadata.guild, guild.get());
        self.overwrite_metadata_file(&sounds).await?;
        fs::remove_file(sound.metadata.get_file_path(&self.sounds_dir_path))
            .await
            .map_err(|_| SoundboardError::DeleteFailed)
    }

    pub async fn rename(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
        new_name: String,
    ) -> Result<bool, SoundboardError> {
        let name_regex = match_regex(name);

        // Check if it's the same name.
        if name_regex.is_match(&new_name) {
            return Ok(false);
        }

        let group_regex = group.map(match_regex);
        let mut sounds = self.sounds.lock().await;

        // Get sound id. We must resolve ambiguity first.
        let mut matching = sounds.values().filter_map(|sound| {
            (sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && group_regex
                    .as_ref()
                    .map(|rg| rg.is_match(&sound.metadata.group))
                    .unwrap_or(true))
            .then(|| (sound.metadata.id, sound.metadata.group.clone()))
        });
        let (id, group) = matching.next().ok_or(SoundboardError::SoundNotFound)?;
        if matching.next().is_some() {
            return Err(SoundboardError::SoundNameAmbiguous);
        }

        // Check if a sound with the requested name already exists.
        let new_name_regex = match_regex(&new_name);
        if sounds.values().any(|sound| {
            sound.metadata.guild == guild.get()
                && new_name_regex.is_match(&sound.metadata.name)
                && sound.metadata.group == group
        }) {
            return Err(SoundboardError::NameTaken);
        }

        sounds
            .get_mut(&id)
            .ok_or(SoundboardError::SoundNotFound)?
            .metadata
            .name = new_name;
        self.overwrite_metadata_file(&sounds).await?;
        Ok(true)
    }

    pub async fn move_group(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
        mut new_group: String,
    ) -> Result<bool, SoundboardError> {
        let name_regex = match_regex(name);
        let new_group_regex = match_regex(&new_group);
        let mut sounds = self.sounds.lock().await;

        // Check if the name is already taken in the target group.
        if sounds.values().any(|sound| {
            sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && new_group_regex.is_match(&sound.metadata.group)
        }) {
            return Err(SoundboardError::NameTaken);
        }

        // Find similar existing group.
        new_group = sounds
            .values()
            .find_map(|sound| {
                new_group_regex
                    .is_match(&sound.metadata.group)
                    .then(|| sound.metadata.group.clone())
            })
            .unwrap_or(new_group);

        // Find new position.
        let index = sounds
            .values()
            .filter_map(|s| {
                (s.metadata.guild == guild.get() && s.metadata.group == new_group)
                    .then_some(s.metadata.index)
            })
            .max()
            .map(|i| i + 1)
            .unwrap_or(0);

        // Find requested sound to change.
        let group_regex = group.map(match_regex);
        let mut matching = sounds.values_mut().filter(|sound| {
            sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && group_regex
                    .as_ref()
                    .map(|rg| rg.is_match(&sound.metadata.group))
                    .unwrap_or(true)
        });
        let sound = matching.next().ok_or(SoundboardError::SoundNotFound)?;
        if matching.next().is_some() {
            return Err(SoundboardError::SoundNameAmbiguous);
        }

        // Check if old and new groups are the same.
        if new_group_regex.is_match(&sound.metadata.group) {
            return Ok(false);
        }

        sound.metadata.group = new_group;
        sound.metadata.index = index;
        self.overwrite_metadata_file(&sounds).await?;
        Ok(true)
    }

    async fn change_sound_field<R, F: FnOnce(&mut Sound) -> (R, bool)>(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
        f: F,
    ) -> Result<R, SoundboardError> {
        let name_regex = match_regex(name);
        let group_regex = group.map(match_regex);

        let mut sounds = self.sounds.lock().await;
        let mut matching = sounds.values_mut().filter(|sound| {
            sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && group_regex
                    .as_ref()
                    .map(|rg| rg.is_match(&sound.metadata.group))
                    .unwrap_or(true)
        });
        let sound = matching.next().ok_or(SoundboardError::SoundNotFound)?;
        if matching.next().is_some() {
            return Err(SoundboardError::SoundNameAmbiguous);
        }

        let (res, overwrite) = f(sound);
        if overwrite {
            self.overwrite_metadata_file(&sounds).await?;
        }
        Ok(res)
    }

    pub async fn change_color(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
        color: ButtonStyle,
    ) -> Result<bool, SoundboardError> {
        self.change_sound_field(guild, name, group, |s| {
            if s.metadata.color == color {
                (false, false)
            } else {
                s.metadata.color = color;
                (true, true)
            }
        })
        .await
    }

    pub async fn change_emoji(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
        emoji: String,
    ) -> Result<bool, SoundboardError> {
        self.change_sound_field(guild, name, group, |s| {
            if s.metadata.emoji.as_ref() == Some(&emoji) {
                (false, false)
            } else {
                s.metadata.emoji = Some(emoji);
                (true, true)
            }
        })
        .await
    }

    pub async fn get_id(
        &self,
        guild: GuildId,
        name: &str,
        group: Option<&str>,
    ) -> Result<Ulid, SoundboardError> {
        let name_regex = match_regex(name);
        let group_regex = group.map(match_regex);

        let sounds = self.sounds.lock().await;
        let mut matching = sounds.iter().filter_map(|(id, sound)| {
            (sound.metadata.guild == guild.get()
                && name_regex.is_match(&sound.metadata.name)
                && group_regex
                    .as_ref()
                    .map(|rg| rg.is_match(&sound.metadata.group))
                    .unwrap_or(true))
            .then_some(*id)
        });
        let id = matching.next().ok_or(SoundboardError::SoundNotFound)?;
        if matching.next().is_some() {
            return Err(SoundboardError::SoundNameAmbiguous);
        }

        Ok(id)
    }

    pub async fn random_id(&self, guild: GuildId) -> Option<Ulid> {
        self.sounds
            .lock()
            .await
            .values()
            .filter_map(|sound| (sound.metadata.guild == guild.get()).then_some(sound.metadata.id))
            .choose(&mut rand::rng())
    }

    pub async fn random_id_in_group(&self, guild: GuildId, group_hash: u64) -> Option<Ulid> {
        self.sounds
            .lock()
            .await
            .values()
            .filter_map(|sound| {
                if sound.metadata.guild != guild.get() {
                    return None;
                }
                let mut hasher = DefaultHasher::new();
                sound.metadata.group.hash(&mut hasher);
                (hasher.finish() == group_hash).then_some(sound.metadata.id)
            })
            .choose(&mut rand::rng())
    }

    pub async fn latest_id(&self, guild: GuildId) -> Option<Ulid> {
        self.sounds
            .lock()
            .await
            .values()
            .filter_map(|sound| (sound.metadata.guild == guild.get()).then_some(sound.metadata.id))
            .max()
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
            .filter(|sound| sound.metadata.guild == guild.get())
            .into_group_map_by(|sound| &sound.metadata.group)
            .into_iter()
            .sorted_by(|(g1, _), (g2, _)| g1.cmp(g2))
            .map(|(group, mut sounds)| {
                sounds.sort_by_key(|s| s.metadata.index);
                let sounds = sounds
                    .into_iter()
                    .map(|sound| {
                        json!({
                            "id": sound.metadata.id.to_string(),
                            "name": sound.metadata.name,
                            "emoji": sound.metadata.emoji,
                            "color": button::as_str(sound.metadata.color),
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
        for sound in sounds
            .values_mut()
            .filter(|sound| sound.metadata.guild == guild.get())
        {
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
    pub emoji: Option<String>,
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
    #[error("A sound with the same name in this group already exists.")]
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
    #[error("Sound name is ambiguous. Try to add a group too.")]
    SoundNameAmbiguous,
    #[error("Failed to delete sound.")]
    DeleteFailed,
    #[error("Failed to create backup.")]
    BackupFailed,
}

fn match_regex(searching: &str) -> Regex {
    Regex::new(&format!("(?i)^{}$", regex::escape(searching))).expect("Failed to build match regex")
}

fn search_regex(searching: &str) -> Regex {
    Regex::new(&format!("(?i){}", regex::escape(searching))).expect("Failed to build search regex")
}

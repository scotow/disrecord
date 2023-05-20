[![Release](https://img.shields.io/github/v/tag/scotow/disrecord?label=version)](https://github.com/scotow/disrecord/tags)
[![Build Status](https://img.shields.io/github/actions/workflow/status/scotow/disrecord/docker.yml)](https://github.com/scotow/disrecord/actions)


![Logo](banner.png)

## Features

### Recorder:

- Record users' voice in Discord channels,
- Whitelist
- WAV download
- Customizable buffer duration
- Ring buffer

### Soundboard:

- Create soundboard dashboard
- Supports groups, emojis, button color
- Optional transcoding
- Download sounds

## Configuration

### Options

```
Usage: disrecord [OPTIONS] --discord-token <DISCORD_TOKEN>

Options:
  -v, --verbose...                                           
  -t, --discord-token <DISCORD_TOKEN>                        
  -w, --record-whitelist-path <RECORD_WHITELIST_PATH>        [default: record-whitelist]
  -d, --voice-buffer-duration <VOICE_BUFFER_DURATION>        [default: 3m]
  -e, --voice-buffer-expiration <VOICE_BUFFER_EXPIRATION>    [default: 5m]
  -s, --soundboard-metadata-path <SOUNDBOARD_METADATA_PATH>  [default: soundboard]
  -S, --sounds-dir-path <SOUNDS_DIR_PATH>                    [default: .]
  -D, --sound-max-duration <SOUND_MAX_DURATION>              [default: 15s]
  -c, --sound-cache-duration <SOUND_CACHE_DURATION>          [default: 3m]
  -f, --ffmpeg-path <FFMPEG_PATH>                            [default: ffmpeg]
  -h, --help                                                 Print help
  -V, --version                                              Print version
```
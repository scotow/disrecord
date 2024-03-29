[![Release](https://img.shields.io/github/v/tag/scotow/disrecord?label=version)](https://github.com/scotow/disrecord/tags)
[![Build Status](https://img.shields.io/github/actions/workflow/status/scotow/disrecord/docker.yml)](https://github.com/scotow/disrecord/actions)


![Banner](banner.png)

## Features

### Recorder:

- Record users' voice in Discord channels
- Whitelist
- WAV download
- Customizable buffer duration
- Ring buffer
- Chunked recordings

### Soundboard:

- Create soundboard dashboard
- Supports groups, emojis, button color
- Optional transcoding
- Download sounds
- Backups as ZIP
- Basic usage logs
- HTTP play sound endpoint

![Soundboard](soundboard.png)

### HTTP endpoints

Some actions can also be trigger using HTTP calls to allow scripting:

```
# Join a voice channel:
/guilds/:guild/channels/:channel/join

# Join the same voice channel of a user:
/guilds/:guild/users/:user/follow

# Play a specific sound:
/guilds/:guild/sounds/:sound/play

# Play a random sound among a list:
/guilds/:guild/sounds/:sound|:sound.../play

# Play a random sound from the soundboard:
/guilds/:guild/sounds/random/play

# Play the latest added sound to the soundboard:
/guilds/:guild/sounds/latest/play

# Play the last played sound (using buttons) from the soundboard:
/guilds/:guild/sounds/last-played/play

# Play the nth last played sound (using buttons) from the soundboard:
/guilds/:guild/sounds/last-played/:offset/play
```


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
  -r, --disable-delete
  -g, --allow-grey 
  -a, --soundboard-http-address <SOUNDBOARD_HTTP_ADDRESS>    [default: 127.0.0.1]
  -p, --soundboard-http-port <SOUNDBOARD_HTTP_PORT>          [default: 8080]
  -h, --help                                                 Print help
  -V, --version                                              Print version
```

### Running locally

```sh
cargo run -- [OPTIONS]
```

#### Dependencies:

- Opus ([`songbird`'s README](https://github.com/serenity-rs/songbird#dependencies))
- [`ffmpeg`](https://ffmpeg.org/download.html) command to transcode audio files on the fly (optional)

### Docker

If you prefer to run Disrecord as a Docker container, you can either build the image yourself using the Dockerfile available in this repo, or you can use the [image](https://github.com/scotow/disrecord/pkgs/container/disrecord%2Fdisrecord) built by the GitHub action.

```
docker run -v disrecord:/data ghcr.io/scotow/disrecord/disrecord:latest -t DISCORD_TOKEN -w /data/record-whitelist -s /data/soundboard -S /data
```

### Binding to all interfaces

By default, Disrecord will only listen on the loopback interface, aka. `127.0.0.1`. If you don't want to host Disrecord behind a reverse proxy or if you are using the Docker image, you should specify the `0.0.0.0` address by using the `-a | --soundboard-http-address` option.

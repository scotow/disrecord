> [!IMPORTANT]
> Those examples are used to migrate some on-disk data rather than proposing real examples.

## `migrate-2023-07-29`

On this day I changed the format for sound metadata to use `String` rather than `char` to store emojis. This script helps to migrate from the former format.

## `json-to-bincode`

When upgrading to `serenity` v0.12 / `songbird` v0.4, they upgraded their `serde` dependencies, which "broke" compatibility of our metadata storage. This tool helps regenerate a unique guild backup (`/backup` command) to a metadata file. If you need/want to migrate all your guilds at once, I suggest *git checkouting* to `disrecord` v0.2.34 and modify the `main` to dump a json file par guild.  
# mcli

A small command-line launcher for Minecraft, written in Rust. It downloads game files and a matching Java runtime, keeps its data separate from the official launcher, and can launch with a Microsoft account or offline.

**NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.**

**You must own a legitimate copy of Minecraft: Java Edition, or have a current entitlement that permits you to play it, to use this launcher to play. This applies to offline play too.** Offline mode does not require a saved account or sign-in; it does not grant a game license.

Data is stored in `~/.minecraft-cli` (`C:\Users\you\.minecraft-cli` on Windows).

## Build

Install [Rust](https://rustup.rs/), then run:

```sh
cargo build --release
```

The executable is at `target/release/mcli` (`mcli.exe` on Windows).

## Quick start

No client-ID configuration is required for existing installations: the launcher defaults to its original application ID so saved sessions can refresh as before. `MCLI_CLIENT_ID` is an optional override for developers using a different authorized Microsoft application. Refresh tokens belong to the application used at sign-in; changing this override can require signing in again. Retaining the original ID for compatibility is not a claim of permission or approval to distribute a launcher under that application identity. Before public distribution, establish authorization for the application ID you use.

```sh
# Sign in, then download and launch using the saved account.
# This also creates a profile named 1.21.1.
mcli account login
mcli run 1.21.1

# Explicit offline play without signing in.
# Missing game files or Java still require a network connection.
mcli run 1.21.1 --offline --username Player

# Modded: installs the loader on first use, then launches like any version.
mcli run Modded --version 1.21.1 --loader fabric
```

## Mod loaders

Fabric, Quilt, Forge and NeoForge are supported. A loader is installed as an
ordinary version (e.g. `fabric-loader-0.20.1-1.21.1`) that inherits from the
vanilla one, and the profile simply stores that version id, so later launches
need no network at all. Put mods in the profile's `mods/` folder
(`~/.minecraft-cli/profiles/<name>/mods`).

```sh
# Create a modded profile (recommended/latest stable loader by default)
mcli profile create Create --version 1.20.1 --loader forge --jvm-args "-Xmx6G -XX:+UseG1GC"
mcli profile create Sodium --version 1.21.1 --loader fabric --loader-version 0.16.9

# See which loader versions exist
mcli version loaders neoforge 1.21.1
mcli version loaders fabric 26.3 --all

# Install without launching
mcli version install 1.21.1 --loader quilt

# Switch an existing profile to another loader (or back to vanilla)
mcli profile edit Sodium --loader vanilla
```

Fabric and Quilt profiles come from their official meta APIs. Legacy Forge
(1.7.10–1.12.2 early builds) is unpacked directly. Modern Forge and NeoForge
patch the game during installation, so mcli downloads their official
installer and runs it headlessly (`--installClient`) with the bundled Java.
Forge is funded by ads on its download page; consider
[supporting it](https://www.patreon.com/LexManos/) if you rely on it.

When `--loader` is given without `--loader-version`, an already installed
build for that game version is reused; run `mcli version install <mc> --loader <name>`
to fetch the newest one.

## Common commands

```sh
# Profiles
mcli profile list
mcli profile create Survival --version 1.21.1 --jvm-args "-Xms4G -Xmx4G -XX:+UseG1GC"
mcli profile edit Survival --fullscreen
mcli profile rename Survival Hardcore   # also moves profiles/Survival -> profiles/Hardcore
mcli profile remove Survival --purge

# Versions (`latest` and `snapshot` follow Mojang's manifest)
mcli version list
mcli version list --local
mcli version install 1.21.1
mcli version uninstall 1.21.1
mcli run Bleeding --version snapshot

# Launch options
mcli run Survival --world "My World"          # straight into a world (1.20+)
mcli run Survival --server play.example.org   # or host:port
mcli run Survival --detach                    # give the terminal back
mcli run Survival --dry-run                   # print the java command only
mcli profile edit Survival --no-fullscreen
mcli verify Survival                         # full corruption check + repair

# Accounts
mcli account list
mcli account switch PlayerName
mcli account logout

# Download an official server JAR
mcli server 1.21.1 -o server.jar
```

Use `mcli --help` or a subcommand with `--help` to see every option.

## Minecraft terms and distribution

Use Minecraft in accordance with the [Minecraft EULA](https://www.minecraft.net/en-us/eula), [Minecraft Usage Guidelines](https://www.minecraft.net/en-us/usage-guidelines), and [Microsoft Services Agreement](https://www.microsoft.com/servicesagreement). Minecraft and its related names and assets belong to their respective owners. This independent launcher does not grant rights to those names, assets, or game software.

This repository contains launcher source, not Minecraft game files. Game files are downloaded from the URLs provided by Mojang's version metadata; Java is downloaded from Adoptium. Do not commit or bundle downloaded client/server JARs, assets, modified game distributions, account data, or authentication tokens with the source or releases. Third-party libraries and runtimes retain their own licenses; preserve their required notices when redistributing them. Downloading a server does not accept its EULA on your behalf; review and accept the applicable terms yourself before running it.

## Local data and limitations

`auth.json` in the data directory stores access and refresh tokens in plaintext. Keep it private, do not share it in bug reports, and use `mcli account logout --all` to remove saved accounts. Explicit `--offline` skips account authentication and uses the profile's username and UUID, including any `--username` or `--uuid` overrides. Online authentication failures stop the launch rather than silently switching to offline play.

Profiles are stored in `profiles.json`. (Before 1.1 they were in `launcher_profiles.json`; mcli migrates that file automatically, because mod-loader installers expect that name to belong to the official launcher.)

`profile remove --purge` deletes saves and files, and is restricted to the managed profiles directory. Custom game directories must be removed manually. JVM argument strings are split on whitespace; quoting paths within `--jvm-args` is not supported. If a profile keeps the stock JVM arguments, versions that publish recommended defaults (26.x) use those instead.

Offline players get the same UUID vanilla derives from the username (`OfflinePlayer:<name>`) unless `--uuid` is set. mcli sends `0` for the telemetry-only `--xuid`/`--clientId` arguments.

## File integrity

Every download is checked against its published checksum while it streams
(SHA-1 for Mojang, Fabric and Quilt files, SHA-256 for the Java runtime) and
is written to a `.part` file that is only renamed once complete, so an
interrupted download never leaves a half-written file behind.

On each launch, mcli checks that every file exists **and has the published
size** (one `stat` per file, ~20 ms in total). Truncated or emptied files are
re-downloaded automatically.

`mcli verify <profile or version>` re-hashes every file the profile uses
(client jar, libraries, natives, assets, log config) against the official
SHA-1s, deletes anything damaged and downloads it again. Files that a
mod-loader installer generates locally cannot be re-downloaded; if one of
those is damaged, verify tells you to reinstall the loader.

## Launch-argument checking

Launch arguments are built from the version metadata itself (`arguments.jvm`/`arguments.game`, or `minecraftArguments` for pre-1.13), including rules for OS, architecture, OS version and features. Placeholder expansion is strict: if any `${...}` is left unresolved, mcli refuses to launch and names it, instead of starting the game with a broken argument.

To validate against every version Mojang has ever published (plus Fabric/Quilt on the latest release):

```sh
cargo test -- --ignored
```

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
mcli run 1.21.1 --offline --username Player --ram 4G
```

## Common commands

```sh
# Profiles
mcli profile list
mcli profile create Survival --version 1.21.1 --ram 4G
mcli profile edit Survival --fullscreen
mcli profile remove Survival --purge

# Versions
mcli version list
mcli version list --local
mcli version install 1.21.1
mcli version uninstall 1.21.1

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

`profile remove --purge` deletes saves and files, and is restricted to the managed profiles directory. Custom game directories must be removed manually. JVM argument strings are split on whitespace; quoting paths within `--jvm-args` is not supported. Mod-loader and version-specific structured launch arguments are not fully supported; compatibility with every Minecraft version is not guaranteed.

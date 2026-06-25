# Notices

NTE DPS TOOL is an independent, community-maintained diagnostics tool. It is not affiliated with, endorsed by, sponsored by, or approved by the NTE game publisher, developer, platform operator, or any related rights holder.

## Use Scope

- Use this project only for local diagnostics, personal research, and noncommercial maintenance.
- Do not use this project to operate a paid service, sell builds, bundle it into a commercial product, or provide commercial analytics without a separate written license.
- Do not publish private traffic captures, decrypted payloads, resource export keys, usmap files, unpacked client assets, or user-specific local paths.

## Game Data And Assets

The repository may contain stable derived resource tables and small UI assets needed by the tool. Game names, character names, icons, screenshots, fonts, data tables, and other client-derived materials remain the property of their respective rights holders.

Before redistributing a build or fork, review the included `res/` files and make sure you have the rights required for your distribution channel. If a public release needs a lower-risk package, prefer shipping code and scripts separately from extracted or derived game assets.

## Security And Privacy

Runtime packet captures and exported debug files can contain local network metadata and gameplay state. Keep generated files under `logs/`, `target/`, `data/`, and other ignored directories out of commits and public reports.

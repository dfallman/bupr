# bupr

**Preset-based mirror backups for the macOS terminal**

Bupr allows you to quickly and safely mirror folders in macOS. A typical use scenario is to backup 
a folder structure on your internal storage to an external drive or a mounted NAS device.

Bupr is preset-based, meaning that instead of writing long shell commands at the 
prompt, you describe each backup job once in a small [TOML file](#configuration), then use the 
preset's name each time you want to run the job.

<p align="center">
  <img width="600" alt="bupr" src="https://github.com/user-attachments/assets/3eb68fb4-54ae-4f83-85cd-2c292069690e" />
</p>

### Example use
You type `bupr dev` at the prompt and your 'dev' preset job runs, 
mirroring your `~/dev` folder to an external USB backup drive. 

Modern apps across programming, video editing, music editing, and 3D modelling create gigabytes of
temporary output that your tools can easily regenerate that you typically don't need or
want to backup. Bupr [makes it easy to exclude the files you don't want](#configuration) to mirror for each present, 
meaning that folders such as `target/`, `debug/`, `node_modules/` and `.build/` can be left out. 

Bupr implements [several safety layers](#safety) to ensure safe file operations. It's written in Rust 
from the ground up and does not depend on external applications such as `rsync`. By design, bupr is a CLI utility and does not by itself
have mechanisms for scheduling backups. However, you can use [launchd](#nightly-backups-with-launchd) 
to set up recurring backups with bupr.

## Contents

- [Why bupr?](#why-bupr)
- [Install](#install)
- [Quick start](#quick-start)
- [Examples](#examples)
- [Configuration](#configuration)
- [Commands](#commands)
- [How a run works](#how-a-run-works)
- [Safety](#safety)
- [Restoring](#restoring)
- [Limitations](#limitations)

## Why bupr?

- **Presets instead of shell scripts.** You describe each backup once in a
  small TOML file, then run it by name. There is no long `rsync` command
  to keep in a script, where one missing `\` can silently break it.
- **It skips only what can be regenerated.** A `target/` folder is skipped
  only when a `Cargo.toml` sits next to it. `node_modules/` needs a
  `package.json`, and `.build/` needs a `Package.swift`. Your `.git`
  folders, lockfiles, `.env` files and gitignored notes are backed up. In
  practice this often turns 100+ GB of project folders into a backup of a
  few GB.
- **You can see what it's doing.** A live dashboard shows progress, speed,
  ETA and the files being copied. `--dry-run` shows the plan without doing
  anything. `--simulate` goes through the whole run, reading every file
  but writing nothing.
- **Safety comes first.** bupr can only write inside the preset's
  destination folder, and several independent layers enforce that. See
  [Safety](#safety).

## Install
Note that bupr is a macOS only application, it does not work on Linux and Windows.

With [Homebrew](https://brew.sh), on Apple silicon or Intel Macs:

```sh
brew install dfallman/tap/bupr
```

Or build it with a [Rust toolchain](https://rustup.rs) (1.88 or newer):

```sh
cargo install --git https://github.com/dfallman/bupr
```

Or from a clone:

```sh
git clone https://github.com/dfallman/bupr
cd bupr
cargo install --path .
```

## Quick start

```sh
bupr init              # write a starter config to ~/.config/bupr/config.toml
bupr edit              # adjust it in $EDITOR; it is checked when you save
bupr dev --dry-run -v  # see exactly what would be copied and deleted
bupr dev               # back up for real
```

Prefer to be asked questions? 

```bupr new``` 

This command walks you through creating a preset. It completes folder names as 
you type (try with `Tab`) and checks the destination while you enter it. 

## Examples

### Back up your code folder

Add a preset to `~/.config/bupr/config.toml`:

```toml
[presets.dev]
description = "All my code"
source      = "~/dev"
destination = "/Volumes/Backup/dev"
rules       = ["dev"]
```

Check the plan first. Nothing is written:

```
$ bupr dev --dry-run
bupr · dev  ~/dev → /Volumes/Backup/dev
  copy       26 files (150.0 MB)
  unchanged  0 files (0 B)
  delete     0 entries (0 B)
  folder     new backup folder
  space      1.3 TB free, 183.8 MB needed
  secrets    1 file(s) such as .env or keys
✓ dev (dry run) · nothing was changed · 0:00
```

Add `-v` to list every path (`+` copy, `d` new folder, `~` link, `-` delete,
`>` rename). When the plan looks right, run it:

```
$ bupr dev
bupr · dev  ~/dev → /Volumes/Backup/dev
  26 files (150.0 MB) to copy · 0 to delete · 0 unchanged
✓ dev · 26 files (150.0 MB) copied, 0 deleted, 0 unchanged · 0:00
```

Later runs copy only what changed.

### Find out what makes a backup big

`bupr audit` shows what a preset includes, what it skips and why. It also
lists large folders that your `.gitignore` files ignore but bupr still
backs up:

```
$ bupr audit dev
dev · ~/dev → /Volumes/Backup/dev
  backs up   26 files (150.0 MB)

  skipped:
        2.0 MB  target/ next to Cargo.toml
      300.0 kB  node_modules/ next to package.json
           0 B  .DS_Store

  largest included folders:
      150.0 MB  player
      150.0 MB  player/recordings
       27.3 kB  player/.git

  large folders your .gitignore files ignore but bupr still backs up:
      150.0 MB  player/recordings   → exclude with "/player/recordings/"
  (these are only hints — gitignored files can matter, e.g. agent docs or .env)
```

To skip that folder, add the suggested pattern:

```toml
[presets.dev]
# …
exclude = ["/player/recordings/"]
```

The next run removes it from the backup, since the backup is a mirror.
A dry run shows this first:

```
$ bupr dev --dry-run
  copy       0 files (0 B)
  unchanged  25 files (27.3 kB)
  delete     2 entries (150.0 MB)
  …
```

### Keep one file inside a skipped folder

`include` wins over rules and excludes. To keep a locally patched package
inside an otherwise skipped `node_modules/`, name the folder in the
pattern:

```toml
[presets.web]
source      = "~/dev/webshop"
destination = "/Volumes/Backup/webshop"
rules       = ["dev"]
include     = ["/node_modules/my-patched-lib/"]
```

Everything else in `node_modules/` is still skipped.

### Several drives and presets

Give each drive and folder its own preset:

```toml
[presets.dev]
source      = "~/dev"
destination = "/Volumes/Backup/dev"
rules       = ["dev"]

[presets.photos]
description = "Photos library"
source      = "~/Pictures"
destination = "/Volumes/Archive/photos"
rules       = []            # copy everything, even .DS_Store

[presets.docs]
source      = "~/Documents"
destination = "/Volumes/Backup/documents"
max_delete  = 50            # ask before deleting more than 50 entries
```

Run one, several, or all of them. `--all` skips presets whose drive
isn't plugged in:

```
$ bupr dev photos
$ bupr --all --quiet
– skipping photos: drive not mounted
✓ dev · 0 files (0 B) copied, 0 deleted, 25 unchanged · 0:00
```

`bupr list` shows where each preset goes, whether its drive is there, and
when it last ran:

```
$ bupr list
  dev     ~/dev → /Volumes/Backup/dev  just now
  photos  ~/Pictures → /Volumes/Archive/photos  (drive not mounted)
```

Naming an unplugged drive directly fails right away. bupr never quietly
writes to your internal disk instead:

```
$ bupr photos
✗ photos · drive not mounted: /Volumes/Archive is not available
```

### Preview a run: `--dry-run` or `--simulate`

Neither changes anything on the backup drive. The difference is how much
work they do.

**`--dry-run` plans the backup and stops.** It scans the source and the
backup and works out what would be copied and deleted. It prints that plan
and exits, without asking any questions or reading any file contents, so
it is fast even for huge folders. Add `-v` to list every path it would
touch.

**`--simulate` goes through the whole run except the writes.** It asks the
same questions a real run would, such as the prompt for deleting more than
the limit. It then opens and reads every file it would copy, with the live
dashboard showing speed and ETA, and discards the bytes. It counts
deletions instead of performing them. The copy worker runs under a sandbox
profile that denies all writes, so the kernel guarantees nothing is
written; the backup folder isn't even created. Because it really reads the
data, it takes about as long as the reading part of a real backup. It also
finds problems a dry run can't see: files you don't have permission to
read, disk read errors, and the real throughput.

| | `--dry-run` | `--simulate` |
|---|---|---|
| Shows the plan | yes (`-v` for every path) | summary line, then the dashboard |
| Asks the usual questions | no | yes, except about free space |
| Reads file contents | no | yes, every file to be copied |
| Finds unreadable files and read errors | only folders it can't list | yes |
| Realistic speed and ETA | no | yes |
| Writes anything | no | no (enforced by the sandbox) |
| Recorded in `bupr log` | yes, marked "(dry run)" | yes, marked "(simulated)" |

Use `--dry-run` to check what a change to your excludes or rules would do.
Use `--simulate` before a first large backup, to find unreadable files and
see how long it will take:

```sh
bupr dev --dry-run -v
bupr photos --simulate
```

Neither counts as the preset's last run in `bupr list` or the menu, and the
two flags can't be combined.

### Nightly backups with launchd

bupr never prompts when it has no terminal, or when you pass `--yes`. It
always takes the safe choice instead: deletions over the limit are skipped,
and a folder it doesn't recognise is left alone. Save this as
`~/Library/LaunchAgents/com.example.bupr.plist`, with your own username and
the path from `which bupr`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.example.bupr</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/you/.cargo/bin/bupr</string>
    <string>--all</string>
    <string>--yes</string>
  </array>
  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key>
    <integer>2</integer>
    <key>Minute</key>
    <integer>30</integer>
  </dict>
  <key>StandardOutPath</key>
  <string>/Users/you/Library/Logs/bupr.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/you/Library/Logs/bupr.log</string>
</dict>
</plist>
```

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.example.bupr.plist
```

If a preset backs up a protected folder such as `~/Documents` or
`~/Pictures`, macOS may block background access to it. Grant the `bupr`
binary Full Disk Access in System Settings → Privacy & Security.

Check how the nightly runs went with `bupr log`:

```
$ bupr log dev -n 3
2026-09-28 02:30  dev        ok                 41 files (12.3 MB), 0 deleted  0:04
2026-09-29 02:30  dev        deletions skipped  8 files (1.1 MB), 0 deleted  0:02
2026-09-30 02:30  dev        ok                 17 files (3.0 MB), 2 deleted  0:03
```

Only one run can use a destination at a time. If you start a manual run
while the nightly one is still going, it stops with exit code 2 rather than
racing it.

### Use bupr in a script

The exit code tells a script what happened:

| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | Finished, but some files failed or deletions were skipped. |
| `2` | Aborted, interrupted, or not run (drive not mounted, bad config, another run in progress). |

With several presets, the exit code is the highest of them.

```sh
bupr --all --yes --quiet || osascript -e 'display notification "Check bupr log" with title "Backup needs attention"'
```

### Back up to the internal disk

By default bupr refuses destinations on the Mac's own disk, because a
backup on the same disk doesn't survive that disk failing. To keep a
second copy there anyway (a staging copy before an upload, say), opt in:

```toml
[presets.staging]
source         = "~/dev/webshop"
destination    = "~/Backups/webshop"
allow_internal = true
```

When the source is on the same APFS volume, files are cloned, so the copy
takes almost no extra space until one side changes.

### Keep secrets off unencrypted drives

bupr warns when `.env` files or keys (`*.pem`, `*.key`, `id_rsa*`, …) are
about to be copied to a drive that isn't known to be encrypted. To make
this a hard stop, set `secrets_require_encryption`. Interactive runs ask
first, and unattended runs abort. Add your own patterns with `secrets`:

```toml
[presets.dev]
# …
secrets                    = ["*.p12", "credentials.json"]
secrets_require_encryption = true
```

## Configuration

Presets live in `~/.config/bupr/config.toml`. The file respects
`$XDG_CONFIG_HOME`, and `--config <path>` points at another file. The menu
lists presets in the order they appear in the file. Only `source` and
`destination` are required.

| Key | Default | Meaning |
|---|---|---|
| `description` | — | Shown in the menu and in `bupr list`. |
| `source` | *(required)* | The folder to back up. `~` is expanded. |
| `destination` | *(required)* | The backup folder. It must be a subfolder on a mounted drive, never a volume root. |
| `rules` | `["junk"]` | Built-in rule packs: `dev`, `junk`, or `[]` to copy everything. |
| `exclude` | `[]` | Extra patterns to skip, in `.gitignore` style. |
| `include` | `[]` | Patterns that are always backed up, even if a rule or exclude matches them. |
| `max_delete` | `200` | Ask before deleting more entries than this in one run. |
| `max_delete_size` | `"10 GB"` | Ask before deleting more data than this in one run. |
| `secrets` | `[]` | Extra secret-file patterns, for the unencrypted-drive warning. |
| `secrets_require_encryption` | `false` | Ask before copying secret files to a drive not known to be encrypted. Unattended runs abort instead. |
| `allow_internal` | `false` | Allow a destination on the internal disk. |

Unknown keys are an error, so a typo such as `exlude` can't silently turn
off an exclude. `bupr edit` checks the file before saving it.

### Patterns

Patterns follow `.gitignore`:

- A trailing `/` matches only folders: `cache/`.
- A leading `/`, or a `/` in the middle, anchors the pattern to the source
  folder: `/big-project/recordings/`. Any other pattern matches at any
  depth: `*.iso`.
- `**` matches any number of folders: `**/gen/apple/Externals/`.
- An `include` looks inside an excluded folder only when it names that
  folder, so excluded folders are never read in full just to find a match.
  `/node_modules/keep.txt` reaches inside the top-level `node_modules/`,
  and `**/node_modules/keep.txt` reaches inside every one. `*.keep` matches
  everywhere else, but not inside `node_modules/`, `target/` or other
  excluded folders.

### Rule packs

Run `bupr rules` for the full list.

- **`junk`** skips OS clutter: `.DS_Store`, AppleDouble `._*` files,
  `.Spotlight-V100`, `.Trashes`, editor swap files and similar.
- **`dev`** includes `junk`, plus build output and dependencies, but only
  when the tool that produced them is provably there:

| Skipped folder | …only when next to |
|---|---|
| `target/` | `Cargo.toml` |
| `node_modules/`, `.turbo/`, `.parcel-cache/`, `coverage/` | `package.json` |
| `.svelte-kit/`, `.next/`, `.nuxt/`, `.wrangler/` | their framework config |
| `.build/` | `Package.swift` |
| `build/` | `*.xcodeproj`, `*.xcworkspace` or `build.gradle(.kts)` |
| `.gradle/`, `Pods/`, `vendor/` | Gradle files, `Podfile`, `composer.json` |
| `.venv/`, `venv/` | `pyproject.toml`, `requirements.txt` or `setup.py` |
| `DerivedData/`, `__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.ruff_cache/` | *(always)* |

**bupr never uses `.gitignore` to skip anything.** People gitignore
private but important files, such as `.env`, agent notes (`CLAUDE.md`,
`.claude/`) and local databases. `bupr audit` uses `.gitignore` only to
suggest excludes, and you decide.

## Commands

```
bupr                       menu: pick a preset with ↑/↓ and Enter
bupr <preset> [<preset>…]  run one or more presets in order
bupr --all                 run every preset whose drive is mounted
bupr <preset> --dry-run    show the plan (add -v to list every path)
bupr <preset> --simulate   full run with live progress; reads everything, writes nothing
bupr list                  presets, drive status and last run
bupr log [<preset>]        run history (-n to show more)
bupr audit <preset>        what gets backed up, what is skipped and why, plus exclude hints
bupr rules                 the built-in rule packs
bupr new                   create a preset interactively
bupr edit                  edit the config in $VISUAL/$EDITOR, checked on save
bupr init                  write a starter config
```

Global flags: `--config <path>`, `--yes` (never prompt; always take the
safe choice), `--quiet`, `--no-color`.

For when to use `--dry-run` and when `--simulate`, see
[Preview a run](#preview-a-run---dry-run-or---simulate).

Every run is recorded in `~/.local/state/bupr/history.jsonl` (respects
`$XDG_STATE_HOME`), which `bupr log` and the menu read.

## How a run works

1. **Preflight.** bupr checks that the source exists and that the
   destination is safe to use. If the drive isn't mounted it stops. It
   never creates the folder on your internal disk instead.
2. **Scan and plan.** bupr compares each file's size and modification
   time. On APFS and HFS+ drives it also compares permissions and extended
   attributes such as Finder tags. Only new and changed files are copied.
   Anything no longer in the source, or now excluded, is deleted from the
   backup, so the destination ends up as an exact mirror.
3. **Confirm, only when needed.** bupr asks before it:
   - deletes more than the configured limits,
   - uses a folder it didn't create,
   - starts with too little free space,
   - copies secret files to a drive that isn't known to be encrypted
     (with `secrets_require_encryption`).
4. **Copy.** macOS copies each file to a temporary name, which is then
   renamed into place, so an interrupted copy never leaves a half-written
   file behind. Permissions, modification times and extended attributes
   are kept, and sparse and compressed files stay that way. Symlinks are
   copied as links and never followed. When something changes type (a
   file becomes a folder, say), the new version is built first and only
   then replaces the old one.
5. **Delete, then finalize.** Deletions run after copying. A file or
   folder bupr couldn't read in the source keeps its existing backup, so a
   read error is never mistaken for a deletion. Finally the drive's cache
   is flushed.

Ctrl-C stops a run cleanly after the current chunk. The partial file is
removed and nothing is deleted. Press Ctrl-C again to quit immediately.

## Safety

bupr writes only inside the running preset's destination folder. It never
changes anything else; everywhere outside that folder it only reads. The
exceptions are its own config, history and lock files. Independent layers
enforce this:

- **Kernel sandbox.** Copying runs in a separate worker process under a
  macOS sandbox profile. The worker can write only to the destination,
  read file contents only in the source, the destination and system
  libraries, and never use the network. Dry runs and simulations can't
  write at all.
- **Capability-based file access.** All writes go through a
  [`cap-std`](https://github.com/bytecodealliance/cap-std) handle on the
  destination folder. It refuses `..`, absolute paths, and symlinks that
  point outside the folder, even one swapped in during a run.
- **Compile-time ban.** Clippy forbids every file-writing API outside two
  small modules, and a test makes sure no other module opts out.
- **Destination checks.** bupr refuses a destination that:
  - is a system folder or inside one (`/usr/local`, `/Library/…`),
  - is your home folder or one of its parents,
  - is a bare volume root,
  - is on a drive that isn't mounted,
  - overlaps the source or another preset's destination, however the
    paths are spelled (symlinks, upper or lower case).
- **Ownership marker.** bupr only mirror-deletes inside a folder that holds
  its `.bupr-dest` marker for that preset. It never deletes a folder that
  belongs to another preset.
- **Identity checks.** Before deleting or replacing anything, bupr checks
  that it is still the same file the scan saw. A source file that turned
  into a symlink since the scan is not copied.
- **Case and Unicode aware.** Names are matched the way APFS matches them,
  so renaming `README.md` → `Readme.md` or `strasse` → `straße` is handled
  as a rename, not as a delete and a copy.
- **Other filesystems are left alone.** A disk image or volume mounted
  inside the source is skipped (with a warning), and one mounted inside
  the destination is never written to or deleted.

The test suite checks all of this. It includes adversarial tests for
symlink escapes, folders swapped mid-run, hostile file names, a drive
unplugged mid-run and a drive that fills up. Every test also checks that a
sentinel folder outside the destination is left byte-identical.

## Restoring

A backup is an ordinary folder with the same layout as the source, plus a
small `.bupr-dest` marker file. There is no special format and no restore
command: copy files back with Finder, or with `ditto`, which keeps
permissions and extended attributes:

```sh
ditto /Volumes/Backup/dev/webshop ~/dev/webshop
```

## Limitations

- Bupr is **macOS only**. It relies on APFS, `sandbox-exec`, `diskutil`, and extended
  attributes for safe operations and is not compatible with Linux or Windows.
- Bupr mirrors files to local and external drives only, it does not support `ssh` or network destinations, although support for this might come in later releases. To mirror a folder to your NAS, mount the NAS in macOS.
- **Mirror mode only**: a file deleted from the source is also deleted from the
  backup. Pair bupr with Time Machine or APFS snapshots if you need to go
  back in time.
- Changes are detected from file metadata, not checksums.
- Hard links are copied as separate files. ACLs and ownership are not
  copied.

## License

[MIT](LICENSE) © Daniel Fallman

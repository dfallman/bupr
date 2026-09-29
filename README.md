# bupr

**Preset-based mirror backups for macOS.**
Type `bupr dev` and your `~/dev` folder is mirrored to your backup drive,
without the gigabytes of `target/`, `node_modules/` and `.build/` output that
can be regenerated, with a live progress view right in your terminal.

```
$ bupr dev
bupr · dev  ~/dev → /Volumes/Backup/dev
  8,214 files (5.4 GB) to copy · 12 to delete · 96,110 unchanged
╭ bupr · dev ────────────────────────────────────── mirror ╮
│ ███████████████████░░░░░░░░░░░░░░  57%   3.1 GB / 5.4 GB │
│ files 8,214/14,002   142.0 MB/s   0:22 → eta 0:17        │
│ unchanged 96,110   deleted 12   errors 0                 │
│                                                          │
│ ▸ webshop/src/lib/components/Timeline.svelte     48.0 kB │
│   player/core/src/decoder.rs                     12.0 kB │
│   player/api/openapi.yaml                         3.0 kB │
╰──────────────────────────────────────────────────────────╯
```

Run `bupr` on its own to pick a preset from an arrow-key menu.

> **Status:** early (v0.1). It is used daily by its author, but expect rough
> edges. It runs on macOS only.

## Why bupr?

- **Presets instead of shell scripts.** You describe each backup once in a
  small TOML file, then run it by name. You no longer keep a long `rsync`
  command in a script, where one missing `\` can silently break it.
- **It skips only what can be regenerated.** A `target/` folder is skipped
  only when a `Cargo.toml` sits next to it. `node_modules/` needs a
  `package.json`, and `.build/` needs a `Package.swift`. Your `.git`
  folders, lockfiles, `.env` files and gitignored agent notes (`CLAUDE.md`,
  `.claude/`, `docs/`) are backed up. In practice this often turns
  100+ GB of project folders into a backup of a few GB.
- **You can see what it's doing.** It shows a live progress bar, speed, ETA
  and the current files. `--dry-run` shows the plan without doing anything.
  `--simulate` goes through the whole run, reading every file but writing
  nothing.
- **Safety comes first.** bupr can only write inside the preset's
  destination folder, and several independent layers enforce that. See
  [Safety](#safety).

## Install

You need macOS and a [Rust toolchain](https://rustup.rs) (1.88 or newer).

```sh
cargo install --git https://github.com/dfallman/bupr
```

Or build it from a clone:

```sh
git clone https://github.com/dfallman/bupr
cd bupr
cargo install --path .
```

## Quick start

```sh
bupr init              # writes a starter config to ~/.config/bupr/config.toml
bupr edit              # adjust it in $EDITOR; it is validated when you save
bupr dev --dry-run -v  # show exactly what would be copied and deleted
bupr dev --simulate    # a full run that reads every file but writes nothing
bupr dev               # back up for real
```

You can also let `bupr new` walk you through creating a preset. It
completes folder names as you type and checks the destination while you
enter it.

## Presets

Presets live in `~/.config/bupr/config.toml`. The file respects
`$XDG_CONFIG_HOME`, and you can point at another file with `--config`. The
menu lists presets in the order they appear in the file.

```toml
[presets.dev]
description = "All my code"
source      = "~/dev"
destination = "/Volumes/Backup/dev"
rules       = ["dev"]
exclude     = ["/big-project/recordings/", "*.iso"]

[presets.photos]
source      = "~/Pictures"
destination = "/Volumes/Backup/photos"
```

Only `source` and `destination` are required.

| Key | Default | Meaning |
|---|---|---|
| `description` | — | Shown in the menu and in `bupr list`. |
| `source` | *(required)* | The folder to back up. `~` is expanded. |
| `destination` | *(required)* | The backup folder. It must be a subfolder on a mounted drive, never a volume root. |
| `rules` | `["junk"]` | Built-in rule packs: `dev`, `junk`, or `[]` to copy everything. |
| `exclude` | `[]` | Extra globs to skip, in gitignore style (see below). |
| `include` | `[]` | Globs that are always backed up, even if a rule or exclude matches them. Only reaches inside an excluded folder that it names (see below). |
| `max_delete` | `200` | Ask before deleting more entries than this in one run. |
| `max_delete_size` | `"10 GB"` | Ask before deleting more data than this in one run. |
| `secrets` | `[]` | Extra secret-file globs, used for the unencrypted-drive warning. |
| `allow_internal` | `false` | Allow a destination on the internal disk. |
| `secrets_require_encryption` | `false` | Ask before copying secret files to a drive that is not known to be encrypted. Unattended runs abort instead. |

Unknown keys are an error, so a typo such as `exlude` can't silently turn
off an exclude.

**Glob syntax** follows `.gitignore`:

- A trailing `/` matches only folders.
- `**` matches any depth.
- A leading `/`, or a `/` in the middle, anchors the pattern to the source
  folder. Any other pattern matches at any depth.
- As in `.gitignore`, an `include` only looks inside an excluded folder
  when it names that folder, so excluded folders are never read in full just
  to find a match. An anchored pattern such as `/node_modules/keep.txt`
  reaches inside the excluded `node_modules`, and
  `**/node_modules/keep.txt` does so at any depth. A pattern without a
  slash, such as `*.keep`, keeps matching files everywhere else but does not
  look inside `node_modules/`, `target/` or other excluded folders.

### Rule packs

Run `bupr rules` to print the full list.

- **`junk`** skips OS clutter: `.DS_Store`, AppleDouble `._*` files,
  `.Spotlight-V100`, `.Trashes`, editor swap files, and similar.
- **`dev`** includes `junk`, plus build output and dependencies, but only
  when the tool that produced them can be proved to be there:

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

**bupr never uses `.gitignore` to skip anything.** People gitignore private
but important files, such as `.env`, agent notes and local databases. Use
`bupr audit <preset>` instead. It lists the largest included folders and
points out big gitignored folders you might want to exclude, and you
decide.

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
bupr edit                  edit the config in $VISUAL/$EDITOR, validated on save
bupr init                  write a starter config
```

Global flags: `--config <path>`, `--yes` (never prompt; always take the safe
choice), `--quiet`, `--no-color`.

## How a run works

1. **Preflight.** bupr checks that the source exists and that the
   destination is safe to use (see below). If the drive isn't mounted it
   stops with *"drive not mounted"*. It never quietly creates the folder on
   your internal disk instead.
2. **Scan and plan.** bupr compares size and modification time. On an
   APFS or HFS+ destination it also compares permissions and extended
   attributes (such as Finder tags), and on APFS the modification time to
   the nanosecond. Only new and changed files are copied. Files that no longer
   exist in the source, or that are now excluded, are deleted from the
   backup. The destination ends up as an exact mirror.
3. **Confirm, only when needed.** bupr asks before it:
   - deletes more than the configured limits,
   - uses a folder it didn't create,
   - starts with too little free space.

   If secret files such as `.env` or keys are about to be copied to a drive
   that is not known to be encrypted, it prints a warning (and asks, with
   `secrets_require_encryption`).
4. **Copy.** Files are copied by the kernel (`copyfile`, or a clone when
   source and backup share an APFS volume) to a temporary name, flushed,
   and renamed into place, so an interrupted copy never leaves a
   half-written file behind. Permissions, modification times and extended
   attributes are preserved, sparse files stay sparse and compressed files
   stay compressed. Symlinks are copied as links and never followed, and a
   source file that changed into a link since the scan is not copied.
   When an entry changes type (a file becomes a folder, say), the new one
   is built under a temporary name and only then takes the old one's
   place.
5. **Delete, then finalize.** Deletions run after the copy phase. A source
   file or folder that could not be read keeps its existing backup, so a
   read error never looks like a deletion. At the end the drive's cache is
   flushed once.

Every run is recorded in `~/.local/state/bupr/history.jsonl`, which respects
`$XDG_STATE_HOME`. `bupr log` and the menu read from it.

### Running unattended

bupr doesn't prompt when it runs with `--yes`, or when it has no terminal
(cron, launchd, pipes). It then always takes the safe choice:

- deletions over the limit are skipped and logged,
- an unfamiliar folder is left alone,
- it prints plain progress lines instead of the dashboard.

Exit codes:

| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | Finished, but with file errors or skipped deletions. |
| `2` | Aborted, interrupted, or not run (for example, drive not mounted or bad config). |

With several presets, the exit code is the highest of them.

Ctrl-C stops a run cleanly after the current chunk. The partial file is
removed and nothing is deleted. Press Ctrl-C a second time to quit
immediately.

Only one run at a time can use a destination: a second `bupr` for the same
folder (a manual run overlapping launchd, say) stops with exit code 2.

## Safety

bupr writes only inside the running preset's destination folder. It
never changes anything else; everywhere outside that folder it only reads.
The one exception is its own config and history files. Independent layers
enforce this:

- **Kernel sandbox.** The copying runs in a separate worker process under
  a macOS sandbox profile, which lets it write only to the destination,
  read file contents only in the source, the destination and system
  libraries, and never use the network. Dry runs and simulations deny all
  writes.
- **Capability-based file access.** All writes go through a
  [`cap-std`](https://github.com/bytecodealliance/cap-std) handle on the
  destination folder. That handle refuses `..`, absolute paths, and
  symlinks that point outside the folder, including a symlink swapped in
  during a run.
- **Compile-time ban.** Clippy forbids every file-writing API outside two
  small modules, and a test checks that no other module opts out.
- **Destination checks.** bupr refuses a destination that:
  - is a system folder or inside one (`/usr/local`, `/Library/…`),
  - is your home folder or one of its parents,
  - is a bare volume root,
  - is on a drive that isn't mounted,
  - overlaps the source or another preset's destination, however the
    paths are spelled (symlinks, case).
- **Ownership marker.** bupr only mirror-deletes inside a folder that holds
  its `.bupr-dest` marker for that preset. It never deletes a folder that
  belongs to another preset.
- **Case and Unicode aware.** Names are matched the way APFS matches them,
  with Unicode normalization and full case folding, so `README.md` →
  `Readme.md` or `strasse` → `straße` is treated as a rename. Before each
  deletion, bupr also checks the file's identity once more.
- **Other filesystems are left alone.** A disk image or volume mounted
  inside the destination is never written to or deleted.

The test suite checks all of this. It includes adversarial tests for
symlink escapes, a folder swapped mid-run, hostile file names and a drive
unplugged mid-run. Every test also checks that a sentinel folder outside
the destination is byte-identical afterwards.

## Limitations

- macOS only. The tool relies on APFS, `sandbox-exec`, `diskutil` and
  extended attributes.
- It backs up to local and external drives only. There is no ssh or
  network destination yet.
- Mirror mode only: a file deleted from the source is also deleted from the
  backup. Pair bupr with Time Machine or snapshots if you need to go back
  in time.
- Changes are detected by size, modification time and (on APFS and HFS+)
  permissions and extended attributes, not checksums.
- Hard links are copied as separate files. ACLs and ownership are not
  copied.

## Development

```sh
./scripts/check.sh    # cargo fmt --check, clippy -D warnings, cargo test
```

The tests need macOS. The sandbox tests run `sandbox-exec` and skip
themselves if it isn't available.

## License

[MIT](LICENSE) © Daniel Fallman

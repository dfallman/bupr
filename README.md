# bupr

Preset-based mirror backups for macOS. `bupr dev` mirrors `~/dev` to your
backup drive, skipping build output that can be regenerated, with a live
inline progress view. `bupr` alone shows a menu of presets.

## Install

    cargo install --path .

## Quick start

    bupr init              # starter config at ~/.config/bupr/config.toml
    bupr edit              # adjust it (validated on save)
    bupr dev --dry-run -v  # what would happen
    bupr dev --simulate    # full run that reads everything but writes nothing
    bupr dev               # back up

Other commands: `bupr list`, `bupr log`, `bupr audit dev`, `bupr rules`,
`bupr new`, `bupr --all`, `bupr dev media`.

## Presets

```toml
[presets.dev]
source      = "~/dev"
destination = "/Volumes/Backup/dev"
rules       = ["dev"]                 # skip target/, node_modules/, .build/ …
exclude     = ["/recorder/downloads/"] # gitignore-style
include     = []                      # force-include; beats everything
max_delete  = 200                     # ask before deleting more
max_delete_size = "10 GB"
```

`rules = ["dev"]` only skips a folder when the tool that made it is
provably there (`target/` next to `Cargo.toml`, `node_modules/` next to
`package.json`, …). `.gitignore` is never used to skip files. Run
`bupr rules` for the full list and `bupr audit dev` for suggestions.

## Safety

bupr only ever writes inside the preset's destination folder:

- every write goes through a capability handle on the destination
  (`cap-std`), which refuses paths or symlinks that lead outside it;
- the copy runs in a separate process under a macOS kernel sandbox that
  denies all writes elsewhere (dry runs and simulations deny all writes);
- write APIs are banned at compile time everywhere but two modules;
- it refuses destinations that are unmounted drives, volume roots, system
  folders, your home folder, or overlap the source;
- it never deletes in a folder it did not create (`.bupr-dest` marker)
  unless you confirm, and never when a copy failed.

Exit codes: 0 clean, 1 finished with errors or skipped deletions,
2 aborted / interrupted / not run.

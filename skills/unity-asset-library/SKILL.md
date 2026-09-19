---
name: unity-asset-library
description: Find and pull individual assets (prefabs, models, textures, materials, audio, scripts) with their dependencies out of the team's .unitypackage library into a Unity project, instead of importing whole packages. Use when a Unity task needs art, audio, VFX or a tool that may already exist in a purchased asset pack, or when the user mentions the asset library / Game Asset LIb share / a Synty, KayKit, Feel, etc. pack.
---

# Unity asset library (`uai`)

The team's purchased Unity packages live on an SMB share and are indexed by `uai`
(https://github.com/vibotaku/unity-asset-indexer, a single native binary). It can search every asset
inside every `.unitypackage`, compute the full dependency closure, and write just those files into a
project's `Assets/` folder with `.meta` files, so guid references stay valid.

`uai` is normally on PATH. If not, look for `~/.cargo/bin/uai` or ask the user to install it from
the GitHub releases. Add `--json` to any command for machine-readable output.

Two ways to run it:

* **Local**: the share is mounted and an index exists (`uai config` shows `library_mounted true`).
* **Remote**: a teammate runs `uai serve`; use `uai --server http://host:7878 ...` (or
  `uai config --set-server URL` once, or `UAI_SERVER`). Everything below works the same, exports are
  streamed from the server. No share, no index needed on this machine.

## Workflow

1. **Check the index is available**
   ```bash
   uai config          # library_mounted must be true, or server must be set; packages > 0
   ```
   If neither works: ask the user to mount `smb://fs01.corp.volatilebytes.com/Shared` and run
   `uai index`, or for the server URL. Do not guess paths.

2. **Find candidates**
   ```bash
   uai search barrel -k prefab                 # kinds: prefab material shader texture model audio animation scene script asset font ui vfx
   uai search "sword swing" -k audio -n 20
   uai search tree -p "Fantasy Kingdom"        # restrict to a package; -P publisher; -e fbx
   uai ls "POLYGON - Dungeon" --tree | head -80   # browse a pack
   ```
   Prefer prefabs over raw fbx files for 3D content: the prefab carries materials and colliders.
   Search words are prefix-matched and CamelCase/underscores split, so `sm env rock` finds `SM_Env_Rock_01`.

3. **Inspect before exporting**
   ```bash
   uai deps <guid-or-path> --tree              # what would come along, grouped by package, plus unresolved guids
   uai info <guid-or-path>                     # direct deps and who uses it
   uai cat <material-or-prefab>                # read the YAML (shader name, texture slots, components)
   uai preview <guid> -o /tmp/preview.png      # look at the thumbnail
   ```
   Read the `unresolved` list: URP/HDRP shader guids or TextMeshPro guids mean the target project must
   have that package installed; guids from packages not in the library mean a missing dependency pack.

4. **Export into the project**
   ```bash
   uai export <ids...> --project /path/to/UnityProject --json
   ```
   * Pass several ids at once to share dependencies and one stream per package.
   * Existing files are skipped (no `--force` unless the user asks). Conflicts (same guid already in the
     project at another path) are reported; do not work around them by renaming guids.
   * If the result lists `scripts`, tell the user: partial script imports can fail to compile because
     scripts reference each other by `using`, not by guid. Options: export the whole Scripts folder of that
     package, or re-export with `--no-scripts` and remove the component in the prefab.
   * Unity picks up the new files on the next focus/refresh; with the Editor open you may trigger
     `AssetDatabase.Refresh()` via the Unity CLI/MCP.

   Alternatives: `--out <dir>` (plain copy) or `--unitypackage <file>` (a slim package for manual import).

## Identifiers

`uai` accepts a 32-hex guid, `Package::Assets/full/path`, an `Assets/...` path, a path suffix, a bare
file name, or `#<id>` from `--json` output. If a name is ambiguous the command exits with the
candidate list; pick the guid.

## Gotchas

* Exports from very large packages (multi-GB audio bundles) re-stream the whole package and can take
  minutes over SMB. `uai cache add "<package>"` once (on the machine with the share) makes later
  exports instant.
* Render pipeline mismatch (URP material into Built-in project) yields pink materials; check the
  package's `unity_version`/description in `uai packages` and the shader guid in `uai deps`.
* The index tracks guid references only. `Resources.Load`, Addressables and `Shader.Find` by name are
  invisible; check scripts with `uai cat` if a prefab looks incomplete.
* The user can browse the same index visually with `uai serve --open`; point them there when they
  want to pick assets by eye.

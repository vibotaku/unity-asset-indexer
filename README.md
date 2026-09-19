# unity-asset-index (`uai`)

Search a library of `.unitypackage` files by asset name, resolve each asset's full dependency
closure (prefab → mesh, material, texture, shader, script, across packages), and extract exactly
those files into a Unity project, with their `.meta` files so every GUID reference keeps working.
No more importing a 2 GB package to use three prefabs.

Works directly against the SMB share (`/Volumes/Shared/Game Asset LIb` by default). Packages are
streamed once for indexing and never fully extracted unless you ask for a local cache.

## How it works

A `.unitypackage` is a gzipped tar with one directory per asset GUID:

```
<guid>/asset          the file bytes (absent for folders)
<guid>/asset.meta     Unity .meta YAML (importer settings, contains the guid)
<guid>/pathname       "Assets/Path/To/File.ext"
<guid>/preview.png    optional thumbnail
```

Unity expresses every cross-asset reference as `{fileID: ..., guid: <32 hex>, type: ...}` inside
YAML assets (prefabs, materials, scenes, controllers, ScriptableObjects) and `.meta` files (for
example a model importer's remapped materials). `uai index` streams every package, records each
asset's path, kind, size and every guid it references, and stores it all in SQLite with FTS5
full-text search. `uai deps` walks that graph across the whole library. `uai export` re-streams only
the packages involved and pulls out the wanted guid directories.

The Asset Store also embeds JSON (title, version, Unity version, publish date, category) in the
gzip header of every package; that is indexed too.

## Install

```bash
cd UnityAssetIndexer
uv venv .venv && uv pip install -e '.[mcp]'      # `.[mcp]` is optional, only for the MCP server
source .venv/bin/activate
uai config                                       # shows library path, db location, stats
uai index                                        # first run streams the whole library (~25 GB); later runs are incremental
```

Settings: `~/.unity-asset-index/config.json` (`library`), or env `UAI_LIBRARY` / `UAI_HOME`,
or `--library` / `--home` flags. The index lives at `~/.unity-asset-index/index.db`.

## Everyday use

```bash
uai packages                                  # what is in the library (version, unity version, counts)
uai search chest -k prefab                    # words are prefix-matched; CamelCase and _ are split
uai search "sm env tree" -p "Fantasy Kingdom" # -p/-P/-k/-e filter by package/publisher/kind/ext
uai ls "KayKit - Dungeon" --tree              # browse one package
uai info chest.prefab                         # metadata, direct deps, who uses it
uai deps chest.prefab --tree                  # transitive closure, grouped by package, unresolved guids listed
uai export chest.prefab --project ~/MyGame    # writes Assets/... + .meta into the project
uai export chest.prefab barrel.prefab --unitypackage ~/Desktop/dungeon-bits.unitypackage
uai export SM_Env_Tree_01.prefab --out /tmp/x --dry-run --json
uai cat Chest.mat                             # print a text asset (YAML / C# / shader)
uai preview chest.prefab -o chest.png         # thumbnail from the package
uai rdeps dungeon_texture.png                 # reverse dependencies
uai cache add "Ultimate Sound FX"             # keep a package fully extracted locally for instant exports
```

Assets can be named by guid, `Package::Assets/full/path`, an `Assets/...` path, a path suffix,
or a bare file name (ambiguous names return the candidates). Every command has `--json`.

### Export behaviour

* Destination is one of `--project <UnityProject>` (validated: needs `Assets/` and `ProjectSettings/`),
  `--out <dir>`, or `--unitypackage <file>` (a slim package you can drag into the Editor).
* Transitive dependencies are included by default (`--no-deps` to disable, `--no-scripts` to leave
  C#/DLL/asmdef files out). Folder `.meta` files for the ancestor directories are written so folder
  guids are preserved (`--no-folders` to skip).
* Existing files are never overwritten unless `--force`. With `--project`, the project's `.meta`
  files are scanned first and any planned asset whose guid already exists at a different path is
  reported as a conflict and skipped, instead of creating a duplicate-guid mess.
* Unresolved guids (Unity built-ins, UPM packages like URP shaders or TextMeshPro, or packages you
  do not own) are listed, not fatal. A small table in `uai/known_guids.json` labels common ones; add
  to it freely.
* Scripts: a prefab's `MonoBehaviour` components pull in their `.cs` files. Those may depend on other
  scripts in the same package via `using`, which is invisible to the guid graph, so the export warns
  when scripts are involved. If the project fails to compile afterwards, export the script's folder
  (`uai ls <pkg> --path Assets/Pkg/Scripts`) or use `--no-scripts` and strip the component.

Exporting from a very large package re-streams the gzip up to the last wanted entry (guid
directories are stored in sorted order, so it stops early when it can). Throughput is bound by the
link to the file server: on Wi-Fi to `fs01` this machine sees roughly 7 MB/s, so the 6 GB sound FX
bundle takes ~15 minutes per export and the first full `uai index` about an hour. Wired Ethernet or
running on a machine next to the server helps a lot; `uai cache add <package>` keeps a package
extracted locally so later exports from it are instant.

## For agents

* **CLI with `--json`**: every command emits stable JSON. `export --json` returns the plan, the files
  written, skipped files, conflicts, unresolved guids and script warnings.
* **Claude Code skill**: `skills/unity-asset-library/SKILL.md` teaches the search → deps → export
  workflow. Symlink or copy it to `~/.claude/skills/unity-asset-library`.
* **MCP server**: `uai mcp` (or `uai-mcp`) serves `list_packages`, `search_assets`,
  `list_package_assets`, `asset_info`, `dependencies`, `export_assets`, `read_text_asset` over stdio.
  Register it with `claude mcp add unity-assets -- /path/to/UnityAssetIndexer/.venv/bin/uai-mcp`.

## Layout

```
uai/unitypackage.py   streaming tar/gzip reader, guid reference scanner, kind classification
uai/indexer.py        library walk, parallel per-package scan, incremental by size+mtime
uai/db.py             SQLite schema, FTS5 search, queries
uai/deps.py           cross-package transitive closure, known-guid labels
uai/exporter.py       export to project / folder / .unitypackage, folder metas, conflict scan, cache
uai/cli.py            `uai` commands
uai/mcp_server.py     MCP stdio server
tests/test_uai.py     end-to-end tests on synthetic packages (python -m unittest)
```

## Notes and limits

* Python's `tarfile` "r|gz" mode chokes on the gzip FEXTRA header that Asset Store packages carry;
  the reader wraps the file in `gzip.GzipFile` instead.
* Only guid references are followed. Runtime lookups (`Resources.Load("name")`, Addressables keys,
  shader names in `Shader.Find`) are not visible in the graph.
* Material → shader references to URP/HDRP/Built-in shaders resolve only if the target project has
  that pipeline installed; exporting a URP material into a Built-in project still yields a pink material.
* Same guid in several packages (publishers reuse shared assets) is handled by preferring the
  referrer's package; the alternatives are reported.
* Some packages ship nested `.unitypackage` files (Synty's URP/HDRP variants, for example). Their
  contents are not indexed; `uai search -e unitypackage` lists them and `uai export --out` extracts
  one so it can be imported by hand or added to the library folder.

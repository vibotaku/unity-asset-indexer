# unity-asset-indexer (`uai`)

Search a library of `.unitypackage` files by asset name, preview assets in a browser, resolve each
asset's full dependency closure (prefab → mesh, material, texture, shader, script, across packages),
and extract exactly those files into a Unity project with their `.meta` files so every GUID reference
keeps working. No more importing a 2 GB package to use three prefabs.

`uai` is a single native binary (Rust) for macOS, Windows and Linux. No runtime, no Python, no
dependencies to install. It ships as:

* a **CLI** (`uai search`, `uai deps`, `uai export`, ...), every command with `--json`;
* an **index server with a web UI** (`uai serve`): search, filter, thumbnails, dependency trees,
  text/image/audio preview, and "download as slim `.unitypackage`";
* a **remote mode** (`uai --server http://host:7878 ...`): client machines use the server's index and
  get exports streamed to them, without mounting the file share or building an index;
* an **MCP server** (`uai mcp`) and a Claude Code / Codex **skill** for agents.

## Install

Download the archive for your platform from the
[Releases page](https://github.com/vibotaku/unity-asset-indexer/releases), unpack it and put `uai`
somewhere on your `PATH`.

| Platform | File |
|---|---|
| macOS Apple Silicon | `uai-<version>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `uai-<version>-x86_64-apple-darwin.tar.gz` |
| Windows x64 | `uai-<version>-x86_64-pc-windows-msvc.zip` |
| Linux x64 / arm64 | `uai-<version>-x86_64-unknown-linux-gnu.tar.gz` / `...-aarch64-...` |

macOS: the binary is not notarized. If Gatekeeper complains the first time, run
`xattr -d com.apple.quarantine uai` once.

From source: `cargo install --git https://github.com/vibotaku/unity-asset-indexer` (Rust 1.85+).

## Quick start

```bash
uai config --set-library "/Volumes/Shared/Game Asset LIb"   # once; or set UAI_LIBRARY
uai index                    # streams every package once; later runs are incremental
uai serve --open             # web UI at http://127.0.0.1:7878
```

Or, if someone on the team already runs a server:

```bash
uai config --set-server http://fs01:7878     # or --server on each call / UAI_SERVER
uai search chest -k prefab
uai export <guid> --project ~/MyGame         # streamed from the server, unpacked locally
```

Settings live in `~/.unity-asset-index/config.json` (`library`, `server`); the index in
`~/.unity-asset-index/index.db`, thumbnails in `previews.db`. Override the directory with
`--home` / `UAI_HOME`.

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
uai preview chest.prefab -o chest.png         # thumbnail
uai rdeps dungeon_texture.png                 # reverse dependencies
uai cache add "Ultimate Sound FX"             # keep a package fully extracted locally for instant exports
```

Assets can be named by guid, `Package::Assets/full/path`, an `Assets/...` path, a path suffix, a bare
file name, or `#<id>`. Ambiguous names return the candidates. Every command has `--json`.

### Export behaviour

* Destination is one of `--project <UnityProject>` (validated: needs `Assets/` and `ProjectSettings/`),
  `--out <dir>`, or `--unitypackage <file>` (a slim package you can drag into the Editor).
* Transitive dependencies are included by default (`--no-deps` to disable, `--no-scripts` to leave
  C#/DLL/asmdef files out). Folder `.meta` files for the ancestor directories are written so folder
  guids are preserved (`--no-folders` to skip).
* Existing files are never overwritten unless `--force`. With `--project`, the project's `.meta`
  files are scanned first and any planned asset whose guid already exists at a different path is
  reported as a conflict and skipped.
* Unresolved guids (Unity built-ins, UPM packages like URP shaders or TextMeshPro, or packages you
  do not own) are listed, not fatal. `src/known_guids.json` labels common ones; add to it freely.
* Scripts: a prefab's `MonoBehaviour` components pull in their `.cs` files. Those may depend on other
  scripts in the same package via `using`, which is invisible to the guid graph, so the export warns
  when scripts are involved. If the project fails to compile afterwards, export the script's folder
  (`uai ls <pkg> --path Assets/Pkg/Scripts`) or use `--no-scripts` and strip the component.

## The server and web UI

```bash
uai serve                       # 127.0.0.1:7878
uai serve --bind 0.0.0.0:7878   # reachable by the team (there is no auth; keep it on the LAN)
```

The UI offers full-text search with kind / package / publisher / extension filters, a thumbnail
grid, a package browser, an asset panel with the dependency tree, reverse dependencies and the text
of YAML / script assets, in-browser viewing of textures and playback of audio clips, and an export
basket that downloads a slim `.unitypackage` (dependencies resolved) for drag-and-drop into Unity.

The JSON API behind it (`/api/search`, `/api/assets/{id}`, `/api/deps`, `/api/export/plan`,
`/api/export/unitypackage`, ...) is what remote mode and the UI use; responses have the same shape
as the CLI's `--json` output.

Thumbnails come from the `preview.png` files inside the packages and are stored during
`uai index`. An index built by an older version has none yet: run `uai index` again (it re-scans
packages that lack thumbnails) or leave it, the asset panel can pull a thumbnail on demand.

## For agents

* **CLI with `--json`**: every command emits stable JSON. `export --json` returns the plan, the files
  written, skipped files, conflicts, unresolved guids and script warnings.
* **Claude Code / Codex skill**: `skills/unity-asset-library/SKILL.md` teaches the
  search → deps → export workflow. Symlink or copy it to `~/.claude/skills/unity-asset-library`
  (Codex: `~/.codex/skills/`).
* **MCP server**: `uai mcp` serves `list_packages`, `search_assets`, `list_package_assets`,
  `asset_info`, `dependencies`, `export_assets`, `read_text_asset` over stdio. Register it with
  `claude mcp add unity-assets -- uai mcp` (add `--server URL` for remote mode).

## How it works

A `.unitypackage` is a gzipped tar with one directory per asset GUID:

```
<guid>/asset          the file bytes (absent for folders)
<guid>/asset.meta     Unity .meta YAML (importer settings, contains the guid)
<guid>/pathname       "Assets/Path/To/File.ext"
<guid>/preview.png    optional thumbnail
```

Unity expresses every cross-asset reference as `{fileID: ..., guid: <32 hex>, type: ...}` inside
YAML assets (prefabs, materials, scenes, controllers, ScriptableObjects) and `.meta` files. `uai index`
streams every package in parallel, records each asset's path, kind, size, every guid it references and
its thumbnail, and stores it all in SQLite with FTS5 full-text search. `uai deps` walks that graph
across the whole library. `uai export` re-streams only the packages involved and pulls out the wanted
guid directories (stopping early when it can, since entries are grouped per guid).

The Asset Store also embeds JSON (title, version, Unity version, publish date, category) in the
gzip header of every package; that is indexed too.

Throughput is bound by the link to the file server: over Wi-Fi at ~7 MB/s a 25 GB library takes
about an hour to index the first time and the multi-GB audio bundles take minutes per export. Run
the server on a machine next to the share, or `uai cache add <package>` for the big ones.

## Layout

```
src/unitypackage.rs   streaming gzip/tar reader, guid reference scanner, kind classification
src/indexer.rs        library walk, parallel per-package scan, incremental by size+mtime, thumbnails
src/db.rs             SQLite schema (compatible with the original Python index), FTS5 search
src/deps.rs           cross-package transitive closure, known-guid labels
src/exporter.rs       export to project / folder / .unitypackage, folder metas, conflict scan, cache
src/resolve.rs        identifier resolution (guid, Package::path, suffix, name, #id)
src/service.rs        the operation set behind CLI / API / MCP, local implementation
src/client.rs         remote implementation (HTTP)
src/server/           axum HTTP API; web/ holds the embedded UI (no build step)
src/mcp.rs            MCP stdio server
src/main.rs           `uai` commands
tests/e2e.rs          end-to-end tests on synthetic packages, including server + remote client
```

`cargo test` runs everything without a share. Releases are built by GitHub Actions on `v*` tags.

## Notes and limits

* Only guid references are followed. Runtime lookups (`Resources.Load("name")`, Addressables keys,
  `Shader.Find`) are not visible in the graph.
* Material → shader references to URP/HDRP/Built-in shaders resolve only if the target project has
  that pipeline installed; exporting a URP material into a Built-in project still yields a pink material.
* Same guid in several packages (publishers reuse shared assets) is handled by preferring the
  referrer's package; the alternatives are reported.
* Some packages ship nested `.unitypackage` files (Synty's URP/HDRP variants, for example). Their
  contents are not indexed; `uai search -e unitypackage` lists them and `uai export --out` extracts
  one so it can be imported by hand or added to the library folder.
* The server has no authentication. Bind it to localhost or a trusted LAN.

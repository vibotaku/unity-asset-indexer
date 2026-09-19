from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path

DEFAULT_LIBRARY = "/Volumes/Shared/Game Asset LIb"


@dataclass
class Config:
    library: Path
    home: Path

    @property
    def db_path(self) -> Path:
        return self.home / "index.db"

    @property
    def cache_dir(self) -> Path:
        return self.home / "cache"

    @property
    def config_file(self) -> Path:
        return self.home / "config.json"


def load_config(library: str | None = None, home: str | None = None) -> Config:
    """Resolve settings. Precedence: explicit args > env (UAI_LIBRARY, UAI_HOME) > config.json > defaults."""
    home_path = Path(home or os.environ.get("UAI_HOME") or (Path.home() / ".unity-asset-index")).expanduser()
    home_path.mkdir(parents=True, exist_ok=True)
    cfg_file = home_path / "config.json"
    data: dict = {}
    if cfg_file.exists():
        try:
            data = json.loads(cfg_file.read_text())
        except json.JSONDecodeError:
            data = {}
    lib = library or os.environ.get("UAI_LIBRARY") or data.get("library") or DEFAULT_LIBRARY
    if not cfg_file.exists():
        cfg_file.write_text(json.dumps({"library": lib}, indent=2) + "\n")
    return Config(library=Path(lib).expanduser(), home=home_path)


def save_library(cfg: Config, library: str) -> None:
    data = {}
    if cfg.config_file.exists():
        try:
            data = json.loads(cfg.config_file.read_text())
        except json.JSONDecodeError:
            data = {}
    data["library"] = library
    cfg.config_file.write_text(json.dumps(data, indent=2) + "\n")

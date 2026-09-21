from __future__ import annotations

import os
from pathlib import Path

import pytest

from flighty_wall.config import ConfigError, load_config, require_private_file


def write_config(path: Path, *, state_path: Path, credentials_path: Path, poll: int = 120) -> None:
    path.write_text(
        f"""
[google]
calendar_id = "friends@example.invalid"
credentials_path = "{credentials_path}"

[service]
poll_interval_seconds = {poll}
lookahead_days = 7

[storage]
state_path = "{state_path}"
""".strip()
        + "\n",
        encoding="utf-8",
    )


def test_load_config_applies_safe_defaults(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    credentials = tmp_path / "service-account.json"
    credentials.write_text("{}", encoding="utf-8")
    credentials.chmod(0o600)
    config_path = tmp_path / "config.toml"
    write_config(config_path, state_path=state_dir / "state.sqlite3", credentials_path=credentials)

    config = load_config(config_path)

    assert config.calendar_id == "friends@example.invalid"
    assert config.poll_interval_seconds == 120
    assert config.lookahead_days == 7
    assert config.dry_run is True
    assert config.max_pages == 10
    assert config.max_events == 500
    assert config.max_field_chars == 8192
    assert config.max_snapshot_bytes == 1_048_576
    assert config.state_path == state_dir / "state.sqlite3"
    assert config.google_credentials_path == credentials


@pytest.mark.parametrize("poll", [0, -1, 29])
def test_load_config_rejects_unsafe_poll_intervals(tmp_path: Path, poll: int) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    credentials = tmp_path / "service-account.json"
    credentials.write_text("{}", encoding="utf-8")
    credentials.chmod(0o600)
    config_path = tmp_path / "config.toml"
    write_config(
        config_path,
        state_path=state_dir / "state.sqlite3",
        credentials_path=credentials,
        poll=poll,
    )

    with pytest.raises(ConfigError, match="poll_interval_seconds"):
        load_config(config_path)


def test_load_config_rejects_primary_calendar(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    credentials = tmp_path / "service-account.json"
    credentials.write_text("{}", encoding="utf-8")
    credentials.chmod(0o600)
    config_path = tmp_path / "config.toml"
    write_config(config_path, state_path=state_dir / "state.sqlite3", credentials_path=credentials)
    config_path.write_text(config_path.read_text().replace("friends@example.invalid", "primary"))

    with pytest.raises(ConfigError, match="dedicated calendar"):
        load_config(config_path)


def test_load_config_rejects_state_parent_that_is_not_a_directory(tmp_path: Path) -> None:
    credentials = tmp_path / "service-account.json"
    credentials.write_text("{}", encoding="utf-8")
    credentials.chmod(0o600)
    invalid_parent = tmp_path / "not-a-directory"
    invalid_parent.write_text("x", encoding="utf-8")
    config_path = tmp_path / "config.toml"
    write_config(
        config_path,
        state_path=invalid_parent / "state.sqlite3",
        credentials_path=credentials,
    )

    with pytest.raises(ConfigError, match="state directory"):
        load_config(config_path)


def test_require_private_file_rejects_group_or_world_access(tmp_path: Path) -> None:
    secret = tmp_path / "secret.json"
    secret.write_text("secret-value", encoding="utf-8")
    secret.chmod(0o644)

    with pytest.raises(ConfigError, match="0600"):
        require_private_file(secret)

    secret.chmod(0o600)
    require_private_file(secret)


def test_config_repr_contains_paths_not_secret_contents(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    credentials = tmp_path / "service-account.json"
    credentials.write_text("super-secret-value", encoding="utf-8")
    credentials.chmod(0o600)
    config_path = tmp_path / "config.toml"
    write_config(config_path, state_path=state_dir / "state.sqlite3", credentials_path=credentials)

    config = load_config(config_path)

    assert "super-secret-value" not in repr(config)
    assert os.fspath(credentials) in repr(config)

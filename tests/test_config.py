"""Tests for safe service configuration loading."""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import TYPE_CHECKING

import pytest

import flighty_wall.config as config_module

if TYPE_CHECKING:
    from pathlib import Path

CONFIG_TEMPLATE = """
[google]
calendar_id = "{calendar_id}"
credentials_path = "{credentials}"

[service]
poll_interval_seconds = {poll}
lookahead_days = 7

[storage]
state_path = "{state}"
"""


@dataclass(frozen=True, slots=True)
class Sandbox:
    """A temporary home holding a 0600 credential file and a writable state directory."""

    root: Path
    credentials: Path
    state_path: Path

    @property
    def config_path(self) -> Path:
        return self.root / "config.toml"

    def write(self, body: str) -> Path:
        self.config_path.write_text(body.strip() + "\n", encoding="utf-8")
        return self.config_path

    def write_default(
        self,
        *,
        poll: int | str = 120,
        calendar_id: str = "friends@example.invalid",
        state: Path | str | None = None,
        credentials: Path | str | None = None,
    ) -> Path:
        return self.write(
            CONFIG_TEMPLATE.format(
                calendar_id=calendar_id,
                credentials=self.credentials if credentials is None else credentials,
                poll=poll,
                state=self.state_path if state is None else state,
            )
        )


@pytest.fixture
def sandbox(tmp_path: Path) -> Sandbox:
    state_directory = tmp_path / "state"
    state_directory.mkdir(mode=0o700)
    credentials = tmp_path / "service-account.json"
    credentials.write_text("{}", encoding="utf-8")
    credentials.chmod(0o600)
    return Sandbox(
        root=tmp_path,
        credentials=credentials,
        state_path=state_directory / "state.sqlite3",
    )


def test_load_config_applies_safe_defaults(sandbox: Sandbox) -> None:
    config = config_module.load_config(sandbox.write_default())

    assert config.google.calendar_id == "friends@example.invalid"
    assert config.google.credentials_path == sandbox.credentials
    assert config.service.poll_interval_seconds == 120
    assert config.service.lookahead_days == 7
    assert config.service.dry_run is True
    assert config.calendar_limits.max_pages == 10
    assert config.calendar_limits.max_events == 500
    assert config.calendar_limits.max_field_chars == 8192
    assert config.calendar_limits.max_snapshot_bytes == 1_048_576
    assert config.storage.state_path == sandbox.state_path


@pytest.mark.parametrize("poll", [0, -1, 29, 86_401])
def test_load_config_rejects_unsafe_poll_intervals(sandbox: Sandbox, poll: int) -> None:
    with pytest.raises(config_module.ConfigError, match="poll_interval_seconds"):
        config_module.load_config(sandbox.write_default(poll=poll))


def test_load_config_rejects_primary_calendar(sandbox: Sandbox) -> None:
    with pytest.raises(config_module.ConfigError, match="dedicated calendar"):
        config_module.load_config(sandbox.write_default(calendar_id="primary"))


def test_load_config_rejects_blank_calendar_id(sandbox: Sandbox) -> None:
    with pytest.raises(config_module.ConfigError, match="calendar_id"):
        config_module.load_config(sandbox.write_default(calendar_id="   "))


def test_load_config_rejects_state_parent_that_is_not_a_directory(sandbox: Sandbox) -> None:
    invalid_parent = sandbox.root / "not-a-directory"
    invalid_parent.write_text("x", encoding="utf-8")

    with pytest.raises(config_module.ConfigError, match="state directory"):
        config_module.load_config(sandbox.write_default(state=invalid_parent / "state.sqlite3"))


def test_load_config_rejects_an_unknown_table(sandbox: Sandbox) -> None:
    sandbox.write_default()
    sandbox.write(sandbox.config_path.read_text(encoding="utf-8") + '\n[flightwall]\nhost = "x.invalid"\n')

    with pytest.raises(config_module.ConfigError, match="flightwall"):
        config_module.load_config(sandbox.config_path)


def test_load_config_rejects_an_unknown_key_in_a_known_table(sandbox: Sandbox) -> None:
    sandbox.write_default()
    sandbox.write(sandbox.config_path.read_text(encoding="utf-8").replace("lookahead_days", "lookahead_dys"))

    with pytest.raises(config_module.ConfigError, match="lookahead_dys"):
        config_module.load_config(sandbox.config_path)


def test_load_config_rejects_a_quoted_integer(sandbox: Sandbox) -> None:
    with pytest.raises(config_module.ConfigError, match="poll_interval_seconds"):
        config_module.load_config(sandbox.write_default(poll='"120"'))


def test_load_config_rejects_an_integer_for_a_boolean(sandbox: Sandbox) -> None:
    sandbox.write_default()
    sandbox.write(
        sandbox.config_path.read_text(encoding="utf-8").replace(
            "lookahead_days = 7",
            "lookahead_days = 7\ndry_run = 1",
        )
    )

    with pytest.raises(config_module.ConfigError, match="dry_run"):
        config_module.load_config(sandbox.config_path)


def test_load_config_rejects_an_empty_path(sandbox: Sandbox) -> None:
    with pytest.raises(config_module.ConfigError, match="state_path"):
        config_module.load_config(sandbox.write_default(state=""))


def test_load_config_expands_a_home_relative_path(
    sandbox: Sandbox,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("HOME", os.fspath(sandbox.root))

    config = config_module.load_config(
        sandbox.write_default(
            credentials="~/service-account.json",
            state="~/state/state.sqlite3",
        )
    )

    assert config.google.credentials_path == sandbox.credentials
    assert config.storage.state_path == sandbox.state_path


def test_require_private_file_rejects_group_or_world_access(tmp_path: Path) -> None:
    secret = tmp_path / "secret.json"
    secret.write_text("secret-value", encoding="utf-8")
    secret.chmod(0o644)

    with pytest.raises(config_module.ConfigError, match="0600"):
        config_module.require_private_file(secret)

    secret.chmod(0o600)
    config_module.require_private_file(secret)


def test_config_repr_contains_paths_not_secret_contents(sandbox: Sandbox) -> None:
    sandbox.credentials.write_text("super-secret-value", encoding="utf-8")

    config = config_module.load_config(sandbox.write_default())

    assert "super-secret-value" not in repr(config)
    assert os.fspath(sandbox.credentials) in repr(config)

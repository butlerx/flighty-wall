"""Tests for the shared redaction rules.

Both fixture writers depend on this module, so every rule is pinned here rather than
re-tested per caller. Each secret below is a fabricated sample, not a real credential.
"""

from __future__ import annotations

from flighty_wall.redaction import (
    REDACTED_BY_KEY,
    as_mapping,
    scrub_mapping,
    scrub_text,
    scrub_value,
)

FAKE_JWT = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1g"
FAKE_OPAQUE = "AKIA6BHTGZ5R4WQ2PLNVX3CDMJ7YKF8ES1U0"


def test_scrubs_email() -> None:
    assert "@" not in scrub_text("ping friend.name@example.invalid about it")


def test_scrubs_url_including_private_calendar_and_deeplink() -> None:
    redacted = scrub_text("open flighty://flight/123 or https://calendar.google.com/x/private-abc")
    assert "flighty://" not in redacted
    assert "calendar.google.com" not in redacted


def test_scrubs_jwt_before_any_other_rule_can_split_it() -> None:
    redacted = scrub_text(f"Authorization: Bearer {FAKE_JWT}")
    assert "eyJ" not in redacted
    assert "<redacted-token>" in redacted


def test_scrubs_uuid() -> None:
    redacted = scrub_text("device 4f3c2a19-7b6e-4d51-9a8f-2c1b0e5d7a63 reported")
    assert "4f3c2a19" not in redacted


def test_scrubs_credential_but_keeps_the_scheme_name_visible() -> None:
    # The scheme is the part of the contract worth keeping; the value never is.
    redacted = scrub_text("api_key=sk-live-9d82hf03mfkq")
    assert "sk-live" not in redacted
    assert "api_key" in redacted


def test_scrubs_long_opaque_token() -> None:
    assert FAKE_OPAQUE not in scrub_text(f"session {FAKE_OPAQUE} ok")


def test_scrubs_booking_and_seat_codes() -> None:
    redacted = scrub_text("Confirmation: XR7K2Q\nSeat: 14F")
    assert "XR7K2Q" not in redacted
    assert "14F" not in redacted


def test_scrubs_precise_coordinates_in_text_and_as_floats() -> None:
    redacted = scrub_text("centre 53.349805,-6.260310")
    assert "53.349805" not in redacted
    assert scrub_value(53.349805) == "<redacted-coordinate>"


def test_keeps_ordinary_numbers_intact() -> None:
    # Radius, altitude, and flight numbers are contract detail, not personal data.
    assert scrub_value(25.0) == 25.0
    assert scrub_value(8721) == 8721
    assert scrub_value(35.5) == 35.5


def test_scrubs_caller_supplied_terms_case_insensitively() -> None:
    redacted = scrub_text("Flight for Alice Smith", ("alice smith",))
    assert "Alice" not in redacted
    assert "<redacted-name>" in redacted


def test_ignores_empty_sensitive_terms() -> None:
    # An empty --redact-term would otherwise match everywhere and destroy the fixture.
    assert scrub_text("DUB-BCN", ("",)) == "DUB-BCN"


def test_scrub_value_recurses_through_lists_and_mappings() -> None:
    scrubbed = scrub_value({"legs": [{"crew": "bob@example.invalid"}]})
    assert "bob@" not in repr(scrubbed)


def test_dropped_keys_are_removed_at_every_depth() -> None:
    scrubbed = scrub_mapping(
        {"outer": {"attendees": ["someone"], "kept": 1}},
        dropped_keys=frozenset({"attendees"}),
    )
    assert scrubbed == {"outer": {"kept": 1}}


def test_redacted_keys_keep_the_name_and_discard_short_secrets() -> None:
    # A six-character token defeats every length-based pattern, so the key name decides.
    scrubbed = scrub_mapping({"token": "abc123", "limit": 5}, redacted_keys=frozenset({"token"}))
    assert scrubbed == {"token": REDACTED_BY_KEY, "limit": 5}


def test_redacted_keys_match_regardless_of_case() -> None:
    scrubbed = scrub_mapping({"Authorization": "Custom xyz"}, redacted_keys=frozenset({"authorization"}))
    assert scrubbed["Authorization"] == REDACTED_BY_KEY


def test_redacted_keys_discard_nested_structure_too() -> None:
    scrubbed = scrub_mapping(
        {"session": {"id": "s1", "user": {"email": "a@b.invalid"}}},
        redacted_keys=frozenset({"session"}),
    )
    assert scrubbed == {"session": REDACTED_BY_KEY}


def test_as_mapping_rejects_non_mappings() -> None:
    assert as_mapping({"a": 1}) == {"a": 1}
    assert as_mapping([1, 2]) is None
    assert as_mapping("text") is None
    assert as_mapping(None) is None

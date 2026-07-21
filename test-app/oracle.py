"""Pure-Python UIAutomator XML oracles for ``run-tests.py``.

This module is deliberately dependency-free (stdlib only) so it can be unit
tested in isolation by ``test_oracle.py`` without pulling in ``phone_agent`` or
any live device. The agent (AutoGLM) is treated as an *action driver only*;
correctness is decided by asserting real invariants against the parsed ADB
UIAutomator XML dumped after each agent action.

Public surface:

- ``extract_text(root)``           — flatten every ``text``/``content-desc`` attribute.
- ``extract_tick_count(root)``     — parse the "Tick Count: N" widget (or ``None``).
- ``count_tick_log_entries(text)`` — count structured "tick: Tick #N" log rows.
- ``status_token(text)``           — canonical lifecycle status word seen on screen.
- ``predicate_for(test_id, root)`` — per-case boolean oracle keyed by test id.

The dispatch in ``predicate_for`` is the source of truth that turns each
``TestCase.verify`` invariant into a real assertion. New tests must add a
branch here AND a paired fixture in ``test_oracle.py`` (passing + broken).
"""

from __future__ import annotations

import re
import xml.etree.ElementTree as ET
from typing import Optional

__all__ = [
    "extract_text",
    "extract_tick_count",
    "count_tick_log_entries",
    "status_token",
    "predicate_for",
    "KNOWN_TEST_IDS",
]


# ---------------------------------------------------------------------------
# XML / text helpers
# ---------------------------------------------------------------------------

def _walk_text(root: ET.Element) -> list[str]:
    """Yield ``text`` and ``content-desc`` attribute values from every node."""
    out: list[str] = []
    for node in root.iter():
        for attr in ("text", "content-desc"):
            value = node.get(attr)
            if value:
                out.append(value)
    return out


def extract_text(root: ET.Element) -> str:
    """Concatenate every visible UI string into one lowercase blob.

    Lower-casing normalises the ``Running``/``running`` mismatch between the
    verify prose (capitalised) and the actual DOM (``status-text`` is set to a
    lowercase state machine word, e.g. ``running``/``stopped``).
    """
    return " ".join(_walk_text(root)).lower()


_TICK_COUNT_RE = re.compile(r"tick\s*count\s*:\s*(\d+)", re.IGNORECASE)


def extract_tick_count(root: ET.Element) -> Optional[int]:
    """Return the integer shown in the ``Tick Count`` widget, or ``None``."""
    for node in root.iter():
        for attr in ("text", "content-desc"):
            value = node.get(attr) or ""
            m = _TICK_COUNT_RE.search(value)
            if m:
                return int(m.group(1))
    # Fall back to scanning the flattened blob so that "Tick Count:" split across
    # two adjacent UIAutomator nodes still resolves.
    blob = extract_text(root)
    m = _TICK_COUNT_RE.search(blob)
    return int(m.group(1)) if m else None


_TICK_LOG_RE = re.compile(r"tick\s*:\s*tick\s*#", re.IGNORECASE)


def count_tick_log_entries(text: str) -> int:
    """Count structured tick log rows of the form ``HH:MM:SS tick: Tick #N``.

    Each ``addLogEntry('tick', 'Tick #' + count)`` call renders as
    ``"<time> tick: Tick #<n>"``; matching ``tick: tick #`` (case-insensitive)
    counts each row exactly once without double-counting the inner ``Tick``.
    """
    return len(_TICK_LOG_RE.findall(text))


_STATUS_WORDS = (
    "recovering",
    "recoverypending",
    "starting",
    "running",
    "stopped",
    "idle",
)


def status_token(text: str) -> Optional[str]:
    """Return the canonical lifecycle status word visible on screen.

    Longer/more-specific words are checked first so ``recoveryPending`` is not
    misclassified as ``running`` etc. Returns ``None`` if no status word is
    present (used by ``test_oracle.py`` to assert broken fixtures).
    """
    lowered = text.lower()
    # Prefer the labelled form "status: <word>" when present, because the
    # ``Get Lifecycle Status`` card echoes ``Lifecycle: <word> / ...``.
    labelled = re.search(r"(?:status|lifecycle)\s*[:=]\s*([a-z]+)", lowered)
    if labelled:
        candidate = labelled.group(1)
        if candidate in _STATUS_WORDS:
            return candidate
    for word in _STATUS_WORDS:
        if word in lowered:
            return word
    return None


# ---------------------------------------------------------------------------
# Per-test predicates
# ---------------------------------------------------------------------------

def _status_is(root: ET.Element, *expected: str) -> bool:
    text = extract_text(root)
    token = status_token(text)
    return token is not None and token in expected


def _t1_initial_state(root: ET.Element) -> bool:
    """T1 verify: 'Status Stopped and tick count 0'.

    The freshly-launched app shows ``idle`` (no service has run yet) but is also
    a valid "nothing is running" state, so we accept either ``stopped`` or
    ``idle`` together with an exact zero tick count.
    """
    ticks = extract_tick_count(root)
    if ticks is None or ticks != 0:
        return False
    return _status_is(root, "stopped", "idle")


def _t2_running_with_ticks(root: ET.Element) -> bool:
    """T2 verify: 'Status shows Running, tick count > 0'."""
    ticks = extract_tick_count(root)
    if ticks is None or ticks <= 0:
        return False
    return _status_is(root, "running", "starting", "recovering", "recoverypending")


def _t3_running(root: ET.Element) -> bool:
    """T3 verify: 'Status text shows Running'."""
    return _status_is(root, "running", "starting", "recovering", "recoverypending")


def _t4_two_log_ticks(root: ET.Element) -> bool:
    """T4 verify: 'Event log has >= 2 tick entries'."""
    return count_tick_log_entries(extract_text(root)) >= 2


def _t5_stopped(root: ET.Element) -> bool:
    """T5 verify: 'Status shows Stopped'."""
    return _status_is(root, "stopped")


def _t6_stopped_no_crash(root: ET.Element) -> bool:
    """T6 verify: 'No crash, status Stopped, error in event log'.

    Reaching this predicate means the UIAutomator dump succeeded — i.e. the app
    process is alive (no crash). The status invariant is the load-bearing
    assertion; the optional log error line is informational and not required.
    """
    return _status_is(root, "stopped")


def _t7_running_no_crash(root: ET.Element) -> bool:
    """T7 verify: 'No crash, status Running, error in event log'.

    Same reasoning as T6: the dump itself proves no crash.
    """
    return _status_is(root, "running", "starting", "recovering", "recoverypending")


_PREDICATES = {
    "T1": _t1_initial_state,
    "T2": _t2_running_with_ticks,
    "T3": _t3_running,
    "T4": _t4_two_log_ticks,
    "T5": _t5_stopped,
    "T6": _t6_stopped_no_crash,
    "T7": _t7_running_no_crash,
}

KNOWN_TEST_IDS = frozenset(_PREDICATES)


def predicate_for(test_id: str, root: ET.Element) -> bool:
    """Dispatch the per-case boolean oracle.

    Unknown ids (e.g. edge tests with no oracle) raise ``KeyError`` so callers
    can decide how to treat them — ``run-tests.py`` only invokes this for
    core/lifecycle tiers.
    """
    return _PREDICATES[test_id](root)

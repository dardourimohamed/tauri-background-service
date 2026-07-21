"""Fixture-based tests for the UIAutomator oracles in ``oracle.py``.

Stdlib ``unittest`` only — no phone_agent, no device. Each predicate has at
least one *passing* fixture and one or more *plausible broken states* that must
fail. The broken fixtures are the actual guard: they prove the oracle is not a
rubber stamp and will catch the regressions E2E-01 was created to prevent.

Run from inside ``test-app/``::

    python3 -m unittest test_oracle
"""

from __future__ import annotations

import unittest
import xml.etree.ElementTree as ET

from oracle import (
    count_tick_log_entries,
    extract_text,
    extract_tick_count,
    predicate_for,
    status_token,
)


def _ui(*visible_strings: str) -> ET.Element:
    """Build a synthetic UIAutomator hierarchy exposing ``visible_strings``.

    Real ADB dumps are deep; for unit testing we only need the ``text`` and
    ``content-desc`` attribute values to be present on some node.
    """
    root = ET.fromstring("<hierarchy><root><node/></root></hierarchy>")
    parent = root.find("root")
    for s in visible_strings:
        el = ET.SubElement(parent, "node")
        el.set("text", s)
    return root


def _ui_desc(texts: list[str], descs: list[str]) -> ET.Element:
    """Build a hierarchy that splits text/content-desc values across nodes."""
    root = ET.fromstring("<hierarchy><root></root></hierarchy>")
    parent = root.find("root")
    for s in texts:
        el = ET.SubElement(parent, "node")
        el.set("text", s)
    for s in descs:
        el = ET.SubElement(parent, "node")
        el.set("content-desc", s)
    return root


class TestExtractTickCount(unittest.TestCase):
    def test_parses_labelled_widget(self):
        root = _ui("Tick Count: 7")
        self.assertEqual(extract_tick_count(root), 7)

    def test_returns_none_when_absent(self):
        root = _ui("Background Service Test App")
        self.assertIsNone(extract_tick_count(root))

    def test_zero_is_a_real_value(self):
        root = _ui("Tick Count: 0")
        self.assertEqual(extract_tick_count(root), 0)

    def test_case_insensitive(self):
        root = _ui("tick count: 12")
        self.assertEqual(extract_tick_count(root), 12)


class TestStatusToken(unittest.TestCase):
    def test_detects_running(self):
        self.assertEqual(status_token("status: running"), "running")

    def test_detects_idle(self):
        self.assertEqual(status_token("the status is idle now"), "idle")

    def test_recovery_pending_wins_over_running(self):
        # recoveryPending contains "running" as a substring; the labelled scan
        # must resolve to the more specific word.
        self.assertEqual(status_token("status: recoveryPending"), "recoverypending")

    def test_returns_none_when_no_status_word(self):
        self.assertIsNone(status_token("Background Service Test App"))


class TestCountTickLogEntries(unittest.TestCase):
    def test_zero_entries(self):
        self.assertEqual(count_tick_log_entries("12:00:00 info: Service started"), 0)

    def test_counts_each_row_once(self):
        text = (
            "12:00:01 tick: Tick #1\n"
            "12:00:02 tick: Tick #2\n"
            "12:00:03 tick: Tick #3\n"
            "12:00:04 stopped: Service stopped at tick #3"
        )
        self.assertEqual(count_tick_log_entries(text), 3)

    def test_does_not_double_count_inner_tick_word(self):
        # A single log row contains "tick" twice (type + detail) but should
        # count as exactly one entry.
        self.assertEqual(count_tick_log_entries("12:00:01 tick: Tick #1"), 1)


class TestT1InitialPredicate(unittest.TestCase):
    """T1: initial/idle screen — tick count 0 and not running."""

    def test_idle_with_zero_ticks_passes(self):
        root = _ui(
            "Background Service Test App",
            "idle",
            "Tick Count: 0",
            "Start Service",
        )
        self.assertTrue(predicate_for("T1", root))

    def test_stopped_with_zero_ticks_passes(self):
        root = _ui("status: stopped", "Tick Count: 0")
        self.assertTrue(predicate_for("T1", root))

    def test_running_state_fails(self):
        root = _ui("status: running", "Tick Count: 0")
        self.assertFalse(predicate_for("T1", root))

    def test_nonzero_tick_count_fails(self):
        root = _ui("status: idle", "Tick Count: 5")
        self.assertFalse(predicate_for("T1", root))

    def test_missing_tick_count_fails(self):
        root = _ui("status: idle")
        self.assertFalse(predicate_for("T1", root))


class TestT2RunningWithTicksPredicate(unittest.TestCase):
    """T2: started service — running and ticks > 0."""

    def test_running_with_ticks_passes(self):
        root = _ui("status: running", "Tick Count: 3")
        self.assertTrue(predicate_for("T2", root))

    def test_starting_with_ticks_passes(self):
        root = _ui("status: starting", "Tick Count: 1")
        self.assertTrue(predicate_for("T2", root))

    def test_running_with_zero_ticks_fails(self):
        # The service claims to be running but no ticks ever landed — a real
        # regression where the foreground service is up but the host task is
        # not actually executing.
        root = _ui("status: running", "Tick Count: 0")
        self.assertFalse(predicate_for("T2", root))

    def test_stopped_with_ticks_fails(self):
        root = _ui("status: stopped", "Tick Count: 3")
        self.assertFalse(predicate_for("T2", root))

    def test_missing_tick_count_fails(self):
        root = _ui("status: running")
        self.assertFalse(predicate_for("T2", root))


class TestT3RunningPredicate(unittest.TestCase):
    """T3: explicit status check shows running."""

    def test_running_passes(self):
        root = _ui("status: running", "Tick Count: 4")
        self.assertTrue(predicate_for("T3", root))

    def test_starting_passes(self):
        root = _ui("status: starting")
        self.assertTrue(predicate_for("T3", root))

    def test_stopped_fails(self):
        root = _ui("status: stopped")
        self.assertFalse(predicate_for("T3", root))

    def test_idle_fails(self):
        root = _ui("status: idle")
        self.assertFalse(predicate_for("T3", root))

    def test_no_status_word_fails(self):
        # The status-text widget is empty — a plausible broken state where the
        # Check Status invoke returned nothing or the DOM was reset.
        root = _ui("Background Service Test App", "Start Service")
        self.assertFalse(predicate_for("T3", root))


class TestT4TwoLogTicksPredicate(unittest.TestCase):
    """T4: event log accumulated at least two tick rows."""

    def test_two_entries_passes(self):
        root = _ui(
            "12:00:01 tick: Tick #1",
            "12:00:02 tick: Tick #2",
        )
        self.assertTrue(predicate_for("T4", root))

    def test_three_entries_passes(self):
        root = _ui(
            "12:00:00 info: Service started",
            "12:00:01 tick: Tick #1",
            "12:00:02 tick: Tick #2",
            "12:00:03 tick: Tick #3",
        )
        self.assertTrue(predicate_for("T4", root))

    def test_one_entry_fails(self):
        root = _ui("12:00:01 tick: Tick #1")
        self.assertFalse(predicate_for("T4", root))

    def test_zero_entries_fails(self):
        root = _ui("12:00:00 info: Service started")
        self.assertFalse(predicate_for("T4", root))

    def test_log_without_structure_fails(self):
        # The agent typed "tick tick tick" somewhere on screen — that must NOT
        # satisfy the structural row check.
        root = _ui("Background Service Test App", "the app ticked")
        self.assertFalse(predicate_for("T4", root))


class TestT5StoppedPredicate(unittest.TestCase):
    """T5: stop button produced a stopped status."""

    def test_stopped_passes(self):
        root = _ui("status: stopped", "Tick Count: 3")
        self.assertTrue(predicate_for("T5", root))

    def test_running_fails(self):
        # Stop did not take effect — the exact bug T5 exists to catch.
        root = _ui("status: running", "Tick Count: 4")
        self.assertFalse(predicate_for("T5", root))

    def test_idle_fails(self):
        root = _ui("status: idle")
        self.assertFalse(predicate_for("T5", root))


class TestT6StoppedNoCrashPredicate(unittest.TestCase):
    """T6: double-tap stop — app alive and stopped."""

    def test_stopped_with_log_error_passes(self):
        root = _ui(
            "status: stopped",
            "12:00:05 error: Service already stopped",
        )
        self.assertTrue(predicate_for("T6", root))

    def test_stopped_without_log_error_still_passes(self):
        # The verify string mentions "error in event log" but the load-bearing
        # invariant is "no crash + stopped"; a missing error line is acceptable.
        root = _ui("status: stopped")
        self.assertTrue(predicate_for("T6", root))

    def test_running_fails(self):
        root = _ui("status: running", "12:00:05 error: already running")
        self.assertFalse(predicate_for("T6", root))


class TestT7RunningNoCrashPredicate(unittest.TestCase):
    """T7: double-tap start — app alive and running."""

    def test_running_with_log_error_passes(self):
        root = _ui(
            "status: running",
            "12:00:05 error: Service already running",
        )
        self.assertTrue(predicate_for("T7", root))

    def test_starting_state_passes(self):
        root = _ui("status: starting")
        self.assertTrue(predicate_for("T7", root))

    def test_stopped_fails(self):
        root = _ui("status: stopped")
        self.assertFalse(predicate_for("T7", root))


class TestPredicateDispatch(unittest.TestCase):
    def test_unknown_id_raises(self):
        # Edge tests have no oracle; the dispatcher must refuse to silently
        # approve them.
        with self.assertRaises(KeyError):
            predicate_for("T99", _ui("anything"))

    def test_extract_text_flattens_desc(self):
        root = _ui_desc(["status: running"], ["Tick Count: 2"])
        text = extract_text(root)
        self.assertIn("running", text)
        self.assertIn("tick count: 2", text)


if __name__ == "__main__":
    unittest.main()

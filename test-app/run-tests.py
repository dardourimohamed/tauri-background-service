#!/usr/bin/env python3
"""AutoGLM test harness for Background Service plugin e2e testing.

Uses PhoneAgent to drive automated test cases against the deployed Tauri app
on Waydroid. AutoGLM is treated as an *action driver only*: correctness is
decided by asserting real invariants against the ADB UIAutomator XML dumped
after each agent action (see ``oracle.py``). Captures screenshots + XML dumps
and generates a markdown test report.

Usage:
    python test-app/run-tests.py [--skip-preflight] [--tests T1,T2,...]

Credential model (E2E-02): ``Z_AI_KEY`` is read from the process environment
FIRST; the local ``.env`` file is consulted only as a fallback when the env
var is unset. The key value is never printed.
"""

import subprocess
import sys
import os
import json
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path
from datetime import datetime, timezone
from dataclasses import dataclass, field, asdict

from oracle import predicate_for, KNOWN_TEST_IDS

try:
    from phone_agent import PhoneAgent
    from phone_agent.agent import AgentConfig
    from phone_agent.model import ModelConfig
except ImportError:  # phone_agent is only needed to actually drive a device.
    PhoneAgent = None  # type: ignore[assignment]
    AgentConfig = None  # type: ignore[assignment]
    ModelConfig = None  # type: ignore[assignment]


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class TestCase:
    id: str           # "T1", "T2", ...
    tier: str         # "core", "lifecycle", "edge"
    instruction: str  # Natural language for AutoGLM
    verify: str       # What to look for in screenshot/result


@dataclass
class TestResult:
    test_id: str
    tier: str
    instruction: str
    agent_response: str
    passed: bool | None  # None = informational
    screenshot_before: str
    screenshot_after: str
    xml_before: str
    xml_after: str
    oracle_detail: str
    error: str | None = None


# ---------------------------------------------------------------------------
# Test case definitions (3 tiers)
# ---------------------------------------------------------------------------

TESTS: list[TestCase] = [
    # Tier 1: Core (must pass)
    TestCase(
        id="T1", tier="core",
        instruction="Open the app named Background Service Test",
        verify="Screenshot shows UI with Status Stopped and tick count 0",
    ),
    TestCase(
        id="T2", tier="core",
        instruction=(
            "Tap the green Start Service button, then wait a few seconds "
            "and verify the status text shows Running and a tick count appears"
        ),
        verify="Status shows Running, tick count > 0",
    ),
    TestCase(
        id="T3", tier="core",
        instruction="Tap the blue Check Status button and verify the status shows Running",
        verify="Status text shows Running",
    ),
    TestCase(
        id="T4", tier="core",
        instruction=(
            "Wait a few seconds, then verify the event log shows at least two "
            "tick events with timestamps"
        ),
        verify="Event log has >= 2 tick entries",
    ),
    TestCase(
        id="T5", tier="core",
        instruction="Tap the red Stop Service button and verify the status shows Stopped",
        verify="Status shows Stopped",
    ),
    # Tier 2: Lifecycle (should pass)
    TestCase(
        id="T6", tier="lifecycle",
        instruction=(
            "Tap the red Stop Service button twice in a row and verify "
            "the app does not crash and the status remains Stopped"
        ),
        verify="No crash, status Stopped, error in event log",
    ),
    TestCase(
        id="T7", tier="lifecycle",
        instruction=(
            "Tap the green Start Service button twice in a row and verify "
            "the app does not crash and the status remains Running"
        ),
        verify="No crash, status Running, error in event log",
    ),
    # Tier 3: Edge cases (informational)
    TestCase(
        id="T8", tier="edge",
        instruction=(
            "Go to Android Settings, find the app named Background Service Test, "
            "force stop it, then go back to the app launcher, reopen the "
            "Background Service Test app, and check if the service is running or stopped"
        ),
        verify="App reopens, reports status after force-stop",
    ),
    TestCase(
        id="T9", tier="edge",
        instruction=(
            "Go to Android Settings, find the app named Background Service Test, "
            "deny the notification permission, then go back to the app, "
            "tap the green Start Service button, and verify the status shows Running"
        ),
        verify="Service starts even without notification permission",
    ),
    TestCase(
        id="T10", tier="edge",
        instruction=(
            "Tap the green Start Service button, wait about 15 seconds for "
            "three tick events, then tap the red Stop Service button, and verify "
            "the service stops cleanly with the final tick count preserved"
        ),
        verify="3+ ticks accumulated, clean stop, tick count preserved",
    ),
]


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
REPORT_DIR = os.path.join(SCRIPT_DIR, "test-report")


def capture_screenshot(path: str) -> None:
    """Capture a screenshot via ADB screencap + pull."""
    subprocess.run(
        ["adb", "shell", "screencap", "-p", "/sdcard/test_step.png"],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["adb", "pull", "/sdcard/test_step.png", path],
        check=True,
        capture_output=True,
    )


# On-device path used by ``adb shell uiautomator dump``. Cleared between uses
# so a stale dump can never satisfy a fresh predicate (E2E-01).
_UIAUTOMATOR_DEVICE_PATH = "/sdcard/test_step_uiautomator.xml"


def capture_uiautomator_xml(local_path: str) -> ET.Element | None:
    """Dump the current window's UIAutomator XML and return its parsed root.

    Returns ``None`` when the dump or pull fails — callers treat that as a
    failed predicate (we have nothing to assert against). A stale on-device
    file is removed first so a previously-captured hierarchy can never satisfy
    a fresh assertion.
    """
    # Clear any stale dump so a fresh failure is not masked by an old success.
    subprocess.run(
        ["adb", "shell", "rm", "-f", _UIAUTOMATOR_DEVICE_PATH],
        capture_output=True,
    )
    dump = subprocess.run(
        ["adb", "shell", "uiautomator", "dump", _UIAUTOMATOR_DEVICE_PATH],
        capture_output=True,
        text=True,
    )
    if dump.returncode != 0:
        return None
    pull = subprocess.run(
        ["adb", "pull", _UIAUTOMATOR_DEVICE_PATH, local_path],
        capture_output=True,
        text=True,
    )
    if pull.returncode != 0:
        return None
    try:
        return ET.parse(local_path).getroot()
    except ET.ParseError:
        return None


def run_cmd(cmd: list[str], label: str) -> subprocess.CompletedProcess:
    """Run a command, returning the CompletedProcess."""
    return subprocess.run(cmd, capture_output=True, text=True)

def load_api_key() -> str:
    """Resolve ``Z_AI_KEY`` from the process env, then a local ``.env`` fallback.

    The process environment wins so CI/rotated credentials are authoritative;
    ``.env`` is only consulted when the env var is unset (E2E-02). The key is
    never logged.
    """
    api_key = os.environ.get("Z_AI_KEY")
    if api_key:
        return api_key
    env_path = Path(__file__).parent / ".env"
    if env_path.exists():
        for line in env_path.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            if k.strip() == "Z_AI_KEY":
                return v.strip().strip('"').strip("'")
    return ""


def preflight_checks(api_key: str) -> None:
    """Verify environment is ready. Exits on failure.

    The AutoGLM probe is now an authenticated GET against ``/models`` so a bad
    or expired key produces an auth-specific failure distinct from a network
    outage (E2E-02). The key itself is never printed.
    """
    print("=== Pre-flight Checks ===\n")

    # 1. Waydroid running
    print("Checking Waydroid status...", end=" ")
    result = run_cmd(["waydroid", "status"], "waydroid")
    if "RUNNING" not in result.stdout:
        print("FAILED")
        print("  Waydroid is not running. Start it with: waydroid session start")
        sys.exit(1)
    print("OK")

    # 2. ADB device connected
    print("Checking ADB connection...", end=" ")
    result = run_cmd(["adb", "devices"], "adb")
    lines = [l for l in result.stdout.strip().split("\n") if l.strip() and "List" not in l]
    devices = [l for l in lines if "device" in l and "offline" not in l]
    if not devices:
        print("FAILED")
        print("  No ADB device connected. Connect with: waydroid adb connect")
        sys.exit(1)
    print(f"OK ({devices[0].split()[0]})")

    # 3. API key present
    print("Checking Z_AI_KEY...", end=" ")
    if not api_key:
        print("FAILED")
        print("  Z_AI_KEY is not set. Export it in the process environment")
        print("  (a local test-app/.env fallback is supported but not preferred).")
        sys.exit(1)
    print("OK (loaded; value suppressed)")

    # 4. AutoGLM API reachable AND authenticated
    print("Checking AutoGLM API (authenticated /models)...", end=" ")
    try:
        req = urllib.request.Request(
            "https://api.z.ai/api/paas/v4/models",
            headers={"Authorization": f"Bearer {api_key}"},
            method="GET",
        )
        urllib.request.urlopen(req, timeout=10)
        print("OK")
    except urllib.error.HTTPError as e:
        print(f"FAILED (HTTP {e.code})")
        if e.code in (401, 403):
            print("  Auth rejected (401/403): Z_AI_KEY is invalid, expired, or")
            print("  lacks the coding/paas scope. Rotate the key and re-run.")
        else:
            print(f"  Unexpected API response. Body: {e.read()[:200]!r}")
        sys.exit(1)
    except urllib.error.URLError as e:
        print(f"FAILED (network: {e.reason})")
        print("  AutoGLM API unreachable. Check network connection.")
        sys.exit(1)
    except Exception as e:
        print(f"FAILED ({type(e).__name__}: {e})")
        print("  AutoGLM API probe failed unexpectedly.")
        sys.exit(1)

    # 5. Report directory
    os.makedirs(REPORT_DIR, exist_ok=True)
    print(f"Report directory: {REPORT_DIR}")

    print("\nAll pre-flight checks passed.\n")


def classify_result(
    test: TestCase,
    response: str,
    xml_root: ET.Element | None,
) -> tuple[bool | None, str]:
    """Decide pass/fail per tier.

    - core/lifecycle: the per-case ``oracle`` predicate against the parsed
      UIAutomator XML is authoritative. The agent response is action-driver
      telemetry only; "Max steps reached" is recorded as a soft signal in the
      detail but cannot pass a failing predicate.
    - edge: always informational (``None``).

    Returns ``(passed, detail)`` so the report can show *why* the oracle ruled
    the way it did.
    """
    if test.tier == "edge":
        return None, "edge tier: informational only"

    if test.id not in KNOWN_TEST_IDS:
        # Defensive: a non-edge test without an oracle is itself a bug — fail
        # closed rather than rubber-stamp.
        return False, f"no oracle registered for {test.id}"

    if xml_root is None:
        return False, "UIAutomator XML dump unavailable; nothing to assert against"

    try:
        ok = predicate_for(test.id, xml_root)
    except KeyError:
        return False, f"no oracle registered for {test.id}"
    except Exception as e:  # pragma: no cover — defensive
        return False, f"oracle raised {type(e).__name__}: {e}"

    detail = "oracle: PASS" if ok else "oracle: FAIL (verify invariant unmet)"
    if response == "Max steps reached":
        detail += "; agent hit max steps"
    return ok, detail


def generate_report(results: list[TestResult]) -> str:
    """Generate markdown test report."""
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")

    # Environment info
    adb_devices = run_cmd(["adb", "devices"], "adb").stdout.strip()
    waydroid_status = run_cmd(["waydroid", "status"], "waydroid").stdout.strip()

    lines: list[str] = []
    lines.append("# Background Service Plugin — E2E Test Report\n")
    lines.append(f"**Date:** {now}\n")
    lines.append("## Environment\n")
    lines.append("```")
    lines.append(f"Waydroid: {waydroid_status}")
    lines.append(f"ADB devices:\n{adb_devices}")
    lines.append("```\n")

    # Summary
    core = [r for r in results if r.tier == "core"]
    lifecycle = [r for r in results if r.tier == "lifecycle"]
    edge = [r for r in results if r.tier == "edge"]

    core_pass = sum(1 for r in core if r.passed is True)
    lifecycle_pass = sum(1 for r in lifecycle if r.passed is True)
    core_total = len(core)
    lifecycle_total = len(lifecycle)

    lines.append("## Summary\n")
    lines.append(f"| Tier | Passed | Total |")
    lines.append(f"|------|--------|-------|")
    lines.append(f"| Core (must pass) | {core_pass} | {core_total} |")
    lines.append(f"| Lifecycle (should pass) | {lifecycle_pass} | {lifecycle_total} |")
    lines.append(f"| Edge (informational) | — | {len(edge)} |\n")

    overall = "PASS" if (core_pass == core_total and lifecycle_pass == lifecycle_total) else "FAIL"
    lines.append(f"**Overall Result: {overall}** (gated on core AND lifecycle)\n")

    # Results table
    lines.append("## Test Results\n")
    lines.append("| ID | Tier | Instruction | Result | Details |")
    lines.append("|----|------|-------------|--------|---------|")

    for r in results:
        if r.passed is None:
            status = "INFO"
        elif r.passed:
            status = "PASS"
        else:
            status = "FAIL"

        detail = r.error or r.oracle_detail or r.agent_response[:80]
        instr = r.instruction[:60] + ("..." if len(r.instruction) > 60 else "")
        lines.append(f"| {r.test_id} | {r.tier} | {instr} | {status} | {detail} |")

    lines.append("")

    # Screenshots + UIAutomator XML dumps
    lines.append("## Screenshots & UIAutomator dumps\n")
    for r in results:
        lines.append(f"### {r.test_id}\n")
        if r.oracle_detail:
            lines.append(f"**Oracle:** {r.oracle_detail}\n")
        if os.path.exists(r.screenshot_before):
            before_rel = os.path.relpath(r.screenshot_before, SCRIPT_DIR)
            lines.append(f"**Before:**\n\n![{r.test_id} before](../{before_rel})\n")
        if os.path.exists(r.screenshot_after):
            after_rel = os.path.relpath(r.screenshot_after, SCRIPT_DIR)
            lines.append(f"**After:**\n\n![{r.test_id} after](../{after_rel})\n")
        if os.path.exists(r.xml_after):
            xml_rel = os.path.relpath(r.xml_after, SCRIPT_DIR)
            lines.append(f"**UIAutomator XML (after):** [`{xml_rel}`](../{xml_rel})\n")

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    import argparse

    parser = argparse.ArgumentParser(description="AutoGLM e2e test harness")
    parser.add_argument(
        "--skip-preflight", action="store_true",
        help="Skip pre-flight checks",
    )
    parser.add_argument(
        "--tests", type=str, default=None,
        help="Comma-separated test IDs to run (e.g. T1,T2,T3)",
    )
    args = parser.parse_args()

    # Resolve the API key BEFORE preflight so the authenticated /models probe
    # can run (env wins; .env is fallback only). E2E-02.
    api_key = load_api_key()

    if not args.skip_preflight:
        preflight_checks(api_key)

    # Filter tests if --tests specified
    tests = TESTS
    if args.tests:
        ids = [t.strip().upper() for t in args.tests.split(",")]
        tests = [t for t in TESTS if t.id in ids]
        if not tests:
            print(f"No matching tests found for: {args.tests}")
            sys.exit(1)

    if not api_key:
        print("Error: Z_AI_KEY not set. Export it in the process environment")
        print("(a local test-app/.env fallback is supported but not preferred).")
        sys.exit(1)

    if PhoneAgent is None:
        print("Error: phone_agent is not installed; cannot drive the device.")
        sys.exit(1)

    # Configure PhoneAgent — CRITICAL: lang="en" on BOTH configs
    model_config = ModelConfig(
        base_url="https://api.z.ai/api/coding/paas/v4",
        api_key=api_key,
        model_name="autoglm-phone-multilingual",
        lang="en",
    )
    agent_config = AgentConfig(
        max_steps=50,
        lang="en",
        verbose=True,
    )
    agent = PhoneAgent(
        model_config=model_config,
        agent_config=agent_config,
    )

    # Ensure report directory exists
    os.makedirs(REPORT_DIR, exist_ok=True)

    # Execute tests
    results: list[TestResult] = []

    print(f"=== Running {len(tests)} tests ===\n")

    for test in tests:
        print(f"--- {test.id} ({test.tier}) ---")
        print(f"Instruction: {test.instruction[:80]}...")

        before_path = os.path.join(REPORT_DIR, f"{test.id}_before.png")
        after_path = os.path.join(REPORT_DIR, f"{test.id}_after.png")
        xml_before_path = os.path.join(REPORT_DIR, f"{test.id}_before.xml")
        xml_after_path = os.path.join(REPORT_DIR, f"{test.id}_after.xml")

        # Capture before screenshot + UIAutomator dump
        try:
            capture_screenshot(before_path)
        except subprocess.CalledProcessError as e:
            print(f"  Warning: before screenshot failed: {e}")
        xml_before_root = capture_uiautomator_xml(xml_before_path)

        # Run test (agent is an action driver only — never an oracle)
        try:
            response = agent.run(test.instruction)
            agent.reset()
            error = None
        except Exception as e:
            response = f"Agent error: {e}"
            error = str(e)
            try:
                agent.reset()
            except Exception:
                pass

        # Capture after screenshot + UIAutomator dump (the oracle input)
        try:
            capture_screenshot(after_path)
        except subprocess.CalledProcessError as e:
            print(f"  Warning: after screenshot failed: {e}")
        xml_after_root = capture_uiautomator_xml(xml_after_path)

        if error:
            passed = False
            oracle_detail = f"agent error: {error}"
        else:
            passed, oracle_detail = classify_result(test, response, xml_after_root)

        result = TestResult(
            test_id=test.id,
            tier=test.tier,
            instruction=test.instruction,
            agent_response=response,
            passed=passed,
            screenshot_before=before_path,
            screenshot_after=after_path,
            xml_before=xml_before_path,
            xml_after=xml_after_path,
            oracle_detail=oracle_detail,
            error=error,
        )
        results.append(result)

        status = "PASS" if passed is True else ("FAIL" if passed is False else "INFO")
        print(f"  Result: {status} — {oracle_detail}")
        print(f"  Agent:  {response[:80]}\n")

    # Generate report
    report = generate_report(results)
    report_path = os.path.join(REPORT_DIR, "report.md")
    with open(report_path, "w") as f:
        f.write(report)

    print(f"\nReport written to: {report_path}")

    # Exit code based on BOTH core AND lifecycle tiers passing (E2E-01).
    core_passed = all(r.passed is True for r in results if r.tier == "core")
    lifecycle_passed = all(r.passed is True for r in results if r.tier == "lifecycle")
    gated_tiers = []
    if not core_passed:
        gated_tiers.append("core")
    if not lifecycle_passed:
        gated_tiers.append("lifecycle")
    if gated_tiers:
        print(f"\nGating tier(s) failed: {', '.join(gated_tiers)}")
    sys.exit(0 if (core_passed and lifecycle_passed) else 1)


if __name__ == "__main__":
    main()

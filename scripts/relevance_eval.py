#!/usr/bin/env python3
"""Relevance eval harness for aish context retrieval.

Usage:
  AISHD_URL=http://127.0.0.1:5033 python3 scripts/relevance_eval.py
  python3 scripts/relevance_eval.py --cases scripts/relevance_eval_cases.json
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any


def http_json(method: str, url: str, payload: dict[str, Any] | None = None) -> Any:
    body: bytes | None = None
    headers = {"Content-Type": "application/json"}
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url=url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            raw = resp.read().decode("utf-8")
            if not raw.strip():
                return {}
            return json.loads(raw)
    except urllib.error.HTTPError as exc:
        text = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{method} {url} failed: HTTP {exc.code} {text}") from exc
    except urllib.error.URLError as exc:
        raise RuntimeError(f"{method} {url} failed: {exc}") from exc


def ensure_health(base_url: str) -> None:
    data = http_json("GET", f"{base_url}/health")
    if data.get("status") != "ok":
        raise RuntimeError(f"unexpected /health response: {data}")


def create_session(base_url: str, title: str) -> str:
    data = http_json("POST", f"{base_url}/v1/sessions", {"title": title})
    sid = data.get("id")
    if not isinstance(sid, str) or not sid:
        raise RuntimeError(f"session create failed: {data}")
    return sid


def write_case_logs(log_root: Path, session_id: str, case: dict[str, Any]) -> Path:
    sdir = log_root / session_id
    sdir.mkdir(parents=True, exist_ok=True)
    logs = case.get("logs", {})

    events = logs.get("events", [])
    if events:
        lines = []
        base_ts = int(case.get("base_ts", 1000))
        for idx, event in enumerate(events):
            e = dict(event)
            e["session_id"] = session_id
            if "ts_ms" not in e:
                e["ts_ms"] = base_ts + idx
            lines.append(json.dumps(e, separators=(",", ":")))
        (sdir / "events.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")

    for key, filename in [
        ("stdin", "stdin.log"),
        ("stdout", "stdout.log"),
        ("stderr", "stderr.log"),
        ("output", "output.log"),
    ]:
        content = logs.get(key)
        if content is None:
            continue
        if isinstance(content, list):
            text = "\n".join(str(x) for x in content) + "\n"
        else:
            text = str(content)
            if not text.endswith("\n"):
                text += "\n"
        (sdir / filename).write_text(text, encoding="utf-8")

    return sdir


def ingest_session(base_url: str, session_id: str) -> dict[str, Any]:
    data = http_json("POST", f"{base_url}/v1/logs/ingest/{session_id}")
    if not isinstance(data, dict):
        raise RuntimeError(f"ingest response invalid: {data}")
    return data


def get_context_bundle(base_url: str, session_id: str, case: dict[str, Any]) -> dict[str, Any]:
    options = case.get("options", {})
    query = case.get("query", "what did i do wrong?")
    params = {
        "q": query,
        "max_lines": str(options.get("max_lines", 120)),
        "max_chars": str(options.get("max_chars", 4500)),
        "output_window": str(options.get("output_window", 1)),
    }
    url = (
        f"{base_url}/v1/logs/context/{session_id}?"
        + urllib.parse.urlencode(params, safe="")
    )
    data = http_json("GET", url)
    if not isinstance(data, dict):
        raise RuntimeError(f"context response invalid: {data}")
    return data


def normalize(text: str) -> str:
    return text.lower()


@dataclass
class CaseResult:
    name: str
    passed: bool
    checks_total: int
    checks_passed: int
    failures: list[str]


def evaluate_bundle(case: dict[str, Any], bundle: dict[str, Any]) -> CaseResult:
    expect = case.get("expect", {})
    checks_total = 0
    checks_passed = 0
    failures: list[str] = []
    name = case.get("name", "unnamed")

    context_text = str(bundle.get("context_text", ""))
    context_lc = normalize(context_text)
    evidence = bundle.get("selected_evidence", [])
    evidence_categories = {
        str(item.get("category")) for item in evidence if isinstance(item, dict)
    }
    evidence_blob = "\n".join(
        str(item.get("text", "")) for item in evidence if isinstance(item, dict)
    )
    evidence_blob_lc = normalize(evidence_blob)

    for needle in expect.get("must_contain", []):
        checks_total += 1
        n = normalize(str(needle))
        if n in context_lc or n in evidence_blob_lc:
            checks_passed += 1
        else:
            failures.append(f"missing text: {needle}")

    for category in expect.get("must_have_categories", []):
        checks_total += 1
        c = str(category)
        if c in evidence_categories:
            checks_passed += 1
        else:
            failures.append(f"missing evidence category: {c}")

    if "min_evidence" in expect:
        checks_total += 1
        minimum = int(expect["min_evidence"])
        if len(evidence) >= minimum:
            checks_passed += 1
        else:
            failures.append(
                f"selected_evidence too small: {len(evidence)} < {minimum}"
            )

    if "max_context_chars" in expect:
        checks_total += 1
        maximum = int(expect["max_context_chars"])
        if len(context_text) <= maximum:
            checks_passed += 1
        else:
            failures.append(f"context_text too long: {len(context_text)} > {maximum}")

    passed = checks_passed == checks_total
    return CaseResult(
        name=name,
        passed=passed,
        checks_total=checks_total,
        checks_passed=checks_passed,
        failures=failures,
    )


def run_case(base_url: str, log_root: Path, case: dict[str, Any]) -> CaseResult:
    name = case.get("name", "unnamed")
    sid = create_session(base_url, f"relevance-eval:{name}")
    write_case_logs(log_root, sid, case)
    ingest_session(base_url, sid)
    bundle = get_context_bundle(base_url, sid, case)
    return evaluate_bundle(case, bundle)


def main() -> int:
    parser = argparse.ArgumentParser(description="aish context relevance eval harness")
    parser.add_argument(
        "--cases",
        default="scripts/relevance_eval_cases.json",
        help="path to JSON cases file",
    )
    parser.add_argument(
        "--url",
        default=os.environ.get("AISHD_URL", "http://127.0.0.1:5033"),
        help="aishd base url",
    )
    parser.add_argument(
        "--log-root",
        default=os.environ.get("AISH_LOG_DIR", os.path.expanduser("~/.local/share/aish/logs")),
        help="log root dir used by aishd",
    )
    args = parser.parse_args()

    cases_path = Path(args.cases)
    if not cases_path.exists():
        print(f"cases file not found: {cases_path}", file=sys.stderr)
        return 2

    try:
        ensure_health(args.url.rstrip("/"))
    except Exception as exc:
        print(f"health check failed: {exc}", file=sys.stderr)
        return 2

    cases = json.loads(cases_path.read_text(encoding="utf-8"))
    if not isinstance(cases, list) or not cases:
        print("cases file must be a non-empty JSON array", file=sys.stderr)
        return 2

    log_root = Path(args.log_root).expanduser()
    log_root.mkdir(parents=True, exist_ok=True)

    results: list[CaseResult] = []
    for case in cases:
        if not isinstance(case, dict):
            continue
        try:
            result = run_case(args.url.rstrip("/"), log_root, case)
        except Exception as exc:
            result = CaseResult(
                name=str(case.get("name", "unnamed")),
                passed=False,
                checks_total=1,
                checks_passed=0,
                failures=[str(exc)],
            )
        results.append(result)
        status = "PASS" if result.passed else "FAIL"
        print(
            f"[{status}] {result.name} ({result.checks_passed}/{result.checks_total} checks)"
        )
        for failure in result.failures:
            print(f"  - {failure}")

    total_checks = sum(r.checks_total for r in results)
    passed_checks = sum(r.checks_passed for r in results)
    failed_cases = [r for r in results if not r.passed]

    print(
        f"\nSummary: {len(results)-len(failed_cases)}/{len(results)} cases passed, "
        f"{passed_checks}/{total_checks} checks passed"
    )

    return 1 if failed_cases else 0


if __name__ == "__main__":
    raise SystemExit(main())

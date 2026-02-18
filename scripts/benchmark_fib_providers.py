#!/usr/bin/env python3
"""
Benchmark Fibonacci coding prompt latency via aishd only.

Measures per provider/model:
- TTFT: time to first token (seconds)
- TTC: time to completion (seconds)

Provider discovery rules:
- API key must be present in env
- A matching aishd provider profile must be resolvable from config
  (or matching default openai_compat provider)

This script does NOT call vendor APIs directly; all requests go through:
  POST {AISHD_URL}/v1/completions/stream
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Tuple

TASK_PROMPT = """Implement and test a Fibonacci function in Python.

Requirements:
1) Implement fibonacci(n: int) -> int in `fib.py`.
2) Write unittest tests in `test_fib.py` (base cases, normal values, and negative input).
3) Show exact shell commands to run tests with `python3 -m unittest -q`.
4) Keep response concise and actionable.
"""


@dataclass
class Target:
    key: str
    display_name: str
    model_default: str
    model_env: str
    key_envs: Sequence[str]
    provider_env: str
    preferred_profile_names: Sequence[str]
    base_url_patterns: Sequence[str]
    fallback_base_url: Optional[str] = None
    fallback_completions_path: Optional[str] = None


@dataclass
class BenchResult:
    target: str
    model: str
    provider: str
    status: str  # ok | fail | skip
    ttft_s: Optional[float]
    ttc_s: Optional[float]
    output_chars: int
    error: Optional[str]
    correctness: str = "skip"  # pass | fail | skip
    correctness_detail: Optional[str] = None


TARGETS: List[Target] = [
    Target(
        key="anthropic",
        display_name="Anthropic",
        model_default="claude-opus-4-6",
        model_env="BENCH_MODEL_ANTHROPIC",
        key_envs=("ANTHROPIC_API_KEY",),
        provider_env="BENCH_PROVIDER_ANTHROPIC",
        preferred_profile_names=("anthropic", "claude"),
        base_url_patterns=("anthropic", "claude"),
        fallback_base_url=None,
        fallback_completions_path=None,
    ),
    Target(
        key="openai",
        display_name="OpenAI",
        model_default="gpt-5.2-codex",
        model_env="BENCH_MODEL_OPENAI",
        key_envs=("OPENAI_API_KEY",),
        provider_env="BENCH_PROVIDER_OPENAI",
        preferred_profile_names=("openai",),
        base_url_patterns=("api.openai.com", "openai"),
        fallback_base_url="https://api.openai.com",
        fallback_completions_path="/v1/responses",
    ),
    Target(
        key="zai",
        display_name="Z.ai",
        model_default="glm-5",
        model_env="BENCH_MODEL_ZAI",
        key_envs=("ZAI_API_KEY",),
        provider_env="BENCH_PROVIDER_ZAI",
        preferred_profile_names=("zai", "glm", "z-ai"),
        base_url_patterns=("api.z.ai", "z.ai"),
        fallback_base_url="https://api.z.ai/api/coding/paas/v4",
        fallback_completions_path="/chat/completions",
    ),
    Target(
        key="together",
        display_name="Together",
        model_default="Qwen/Qwen3-Coder-Next-FP8",
        model_env="BENCH_MODEL_TOGETHER",
        key_envs=("TOGETHER_API_KEY",),
        provider_env="BENCH_PROVIDER_TOGETHER",
        preferred_profile_names=("together",),
        base_url_patterns=("api.together.ai", "together.ai"),
        fallback_base_url="https://api.together.ai/v1",
        fallback_completions_path="/chat/completions",
    ),
    Target(
        key="google",
        display_name="Google",
        model_default="gemini-3.0-pro",
        model_env="BENCH_MODEL_GEMINI",
        key_envs=("GEMINI_API_KEY", "GOOGLE_API_KEY"),
        provider_env="BENCH_PROVIDER_GOOGLE",
        preferred_profile_names=("google", "gemini"),
        base_url_patterns=("generativelanguage.googleapis.com", "googleapis.com", "gemini"),
        fallback_base_url="https://generativelanguage.googleapis.com/v1beta/openai",
        fallback_completions_path="/chat/completions",
    ),
    Target(
        key="mistral",
        display_name="Mistral",
        model_default="devstral-small-latest",
        model_env="BENCH_MODEL_MISTRAL",
        key_envs=("MISTRAL_API_KEY",),
        provider_env="BENCH_PROVIDER_MISTRAL",
        preferred_profile_names=("mistral",),
        base_url_patterns=("api.mistral.ai", "mistral.ai"),
        fallback_base_url="https://api.mistral.ai",
        fallback_completions_path="/v1/chat/completions",
    ),
    Target(
        key="cerebras",
        display_name="Cerebras",
        model_default="zai-glm-4.7",
        model_env="BENCH_MODEL_CEREBRAS",
        key_envs=("CEREBRAS_API_KEY",),
        provider_env="BENCH_PROVIDER_CEREBRAS",
        preferred_profile_names=("cerebras",),
        base_url_patterns=("api.cerebras.ai", "cerebras.ai"),
        fallback_base_url="https://api.cerebras.ai",
        fallback_completions_path="/v1/chat/completions",
    ),
]


def _env_nonempty(name: str) -> Optional[str]:
    value = os.getenv(name, "").strip()
    return value or None


def _first_env(names: Sequence[str]) -> Optional[str]:
    for name in names:
        value = _env_nonempty(name)
        if value:
            return value
    return None


def _model_for(target: Target) -> str:
    return _env_nonempty(target.model_env) or target.model_default


def _provider_override(target: Target) -> Optional[str]:
    val = _env_nonempty(target.provider_env)
    if val is None:
        return None
    lowered = val.lower()
    if lowered in ("default", "none", "null"):
        return ""
    return val


def _load_config(path: Path) -> Dict:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return {}


def _iter_profiles(cfg: Dict) -> List[Tuple[str, Dict]]:
    providers = cfg.get("providers") or {}
    profiles = providers.get("openai_compat_profiles") or {}
    if isinstance(profiles, dict):
        return [(str(name), profile if isinstance(profile, dict) else {}) for name, profile in profiles.items()]
    return []


def _default_provider(cfg: Dict) -> Dict:
    providers = cfg.get("providers") or {}
    default = providers.get("openai_compat") or {}
    return default if isinstance(default, dict) else {}


def _match_profile_by_name(target: Target, profiles: List[Tuple[str, Dict]]) -> Optional[str]:
    wanted = {name.lower() for name in target.preferred_profile_names}
    for name, _ in profiles:
        if name.lower() in wanted:
            return name
    return None


def _match_profile_by_url(target: Target, profiles: List[Tuple[str, Dict]]) -> Optional[str]:
    patterns = [p.lower() for p in target.base_url_patterns]
    for name, profile in profiles:
        base_url = str(profile.get("base_url", "")).lower()
        if any(p in base_url for p in patterns):
            return name
    return None


def _default_matches_target(target: Target, cfg: Dict) -> bool:
    default = _default_provider(cfg)
    base_url = str(default.get("base_url", "")).lower()
    if not base_url:
        return False
    patterns = [p.lower() for p in target.base_url_patterns]
    return any(p in base_url for p in patterns)


def resolve_provider_name(target: Target, cfg: Dict) -> Optional[str]:
    """
    Returns:
      - profile name string => send provider=<name>
      - "" => use default provider (omit provider field)
      - None => unresolved
    """
    override = _provider_override(target)
    if override is not None:
        return override

    profiles = _iter_profiles(cfg)

    by_name = _match_profile_by_name(target, profiles)
    if by_name:
        return by_name

    by_url = _match_profile_by_url(target, profiles)
    if by_url:
        return by_url

    if _default_matches_target(target, cfg):
        return ""

    return None


def _http_request(method: str, url: str, headers: Dict[str, str], body: Optional[Dict] = None):
    data = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(url=url, data=data, method=method, headers=headers)
    return urllib.request.urlopen(req, timeout=300)


def check_health(aishd_url: str) -> Optional[str]:
    url = f"{aishd_url.rstrip('/')}/health"
    try:
        with _http_request("GET", url, headers={"Accept": "application/json"}) as resp:
            if resp.status != 200:
                return f"health returned HTTP {resp.status}"
    except urllib.error.URLError as err:
        return str(err)
    except Exception as err:  # noqa: BLE001
        return str(err)
    return None


def _find_free_port() -> int:
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind(("127.0.0.1", 0))
            return int(sock.getsockname()[1])
    except OSError:
        # Constrained environments may block local bind probes; use a deterministic fallback.
        return 6133 + (os.getpid() % 300)


def _wait_health(url: str, timeout_s: float = 12.0) -> bool:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if check_health(url) is None:
            return True
        time.sleep(0.15)
    return False


def _run_with_temp_aishd_for_target(
    *,
    target: Target,
    model: str,
    max_tokens: int,
) -> Tuple[BenchResult, str]:
    if not target.fallback_base_url or not target.fallback_completions_path:
        return (
            BenchResult(
                target=target.key,
                model=model,
                provider="-",
                status="skip",
                ttft_s=None,
                ttc_s=None,
                output_chars=0,
                error=(
                    "no matching profile and no OpenAI-compatible fallback for this provider "
                    "(anthropic native API is not supported by current aishd provider adapter)"
                ),
            ),
            "",
        )

    aishd_bin = shutil.which("aishd")
    if not aishd_bin:
        return (
            BenchResult(
                target=target.key,
                model=model,
                provider="-",
                status="skip",
                ttft_s=None,
                ttc_s=None,
                output_chars=0,
                error="`aishd` not found in PATH for temp fallback daemon",
            ),
            "",
        )

    port = _find_free_port()
    with tempfile.TemporaryDirectory(prefix=f"aish_bench_{target.key}_") as tmp:
        root = Path(tmp)
        cfg_path = root / "aish.json"
        cfg = {
            "server": {"hostname": "127.0.0.1", "port": port},
            "providers": {
                "openai_compat": {
                    "base_url": target.fallback_base_url,
                    "model": model,
                    "completions_path": target.fallback_completions_path,
                    "api_key_env": target.key_envs[0],
                }
            },
            "tools": {"default_policy": "ask"},
        }
        cfg_path.write_text(json.dumps(cfg), encoding="utf-8")
        url = f"http://127.0.0.1:{port}"
        proc = subprocess.Popen(
            [aishd_bin, "--config", str(cfg_path), "--bind", f"127.0.0.1:{port}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=os.environ.copy(),
        )
        try:
            if not _wait_health(url):
                return (
                    BenchResult(
                        target=target.key,
                        model=model,
                        provider="-",
                        status="fail",
                        ttft_s=None,
                        ttc_s=None,
                        output_chars=0,
                        error="temporary aishd did not become healthy in time",
                    ),
                    "",
                )
            result, text = run_stream_benchmark(
                aishd_url=url,
                provider_name="",
                model=model,
                max_tokens=max_tokens,
            )
            result.provider = "default(temp)"
            return result, text
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=2)


def run_stream_benchmark(
    *,
    aishd_url: str,
    provider_name: str,
    model: str,
    max_tokens: int,
) -> Tuple[BenchResult, str]:
    started = time.perf_counter()
    first_token_at: Optional[float] = None
    chars = 0
    chunks: List[str] = []

    body: Dict[str, object] = {
        "messages": [{"role": "user", "content": TASK_PROMPT}],
        "model": model,
        "max_tokens": max_tokens,
        "context_mode": "off",
    }
    provider_label = "default"
    if provider_name:
        body["provider"] = provider_name
        provider_label = provider_name

    url = f"{aishd_url.rstrip('/')}/v1/completions/stream"
    headers = {
        "Content-Type": "application/json",
        "Accept": "text/event-stream",
    }

    event_name = "delta"
    data_lines: List[str] = []

    def handle_event(name: str, data_text: str) -> Tuple[bool, Optional[str]]:
        nonlocal first_token_at, chars
        if not data_text:
            return False, None
        try:
            payload = json.loads(data_text)
        except json.JSONDecodeError:
            return False, None

        if name == "delta":
            delta = payload.get("delta")
            if isinstance(delta, str) and delta:
                chars += len(delta)
                chunks.append(delta)
                if first_token_at is None:
                    first_token_at = time.perf_counter()
        elif name == "done":
            text = payload.get("text")
            if isinstance(text, str) and text:
                if first_token_at is None:
                    first_token_at = time.perf_counter()
                if not chunks:
                    chars += len(text)
                    chunks.append(text)
            return True, None
        elif name == "error":
            err = payload.get("error")
            if not isinstance(err, str):
                err = json.dumps(payload)
            return True, err
        return False, None

    try:
        with _http_request("POST", url, headers=headers, body=body) as resp:
            for raw in resp:
                line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                if line.startswith("event:"):
                    event_name = line[6:].strip() or "delta"
                    continue
                if line.startswith("data:"):
                    data_lines.append(line[5:].lstrip())
                    continue
                if line == "":
                    done, err = handle_event(event_name, "\n".join(data_lines).strip())
                    data_lines = []
                    if done:
                        ended = time.perf_counter()
                        if err:
                            return BenchResult(
                                target="",
                                model=model,
                                provider=provider_label,
                                status="fail",
                                ttft_s=None if first_token_at is None else (first_token_at - started),
                                ttc_s=ended - started,
                                output_chars=chars,
                                error=err,
                            ), "".join(chunks)
                        return BenchResult(
                            target="",
                            model=model,
                            provider=provider_label,
                            status="ok",
                            ttft_s=None if first_token_at is None else (first_token_at - started),
                            ttc_s=ended - started,
                            output_chars=chars,
                            error=None,
                        ), "".join(chunks)
            # EOF without explicit done
            done, err = handle_event(event_name, "\n".join(data_lines).strip())
            ended = time.perf_counter()
            if err:
                return BenchResult("", model, provider_label, "fail", None, ended - started, chars, err), "".join(chunks)
            if done:
                return BenchResult(
                    target="",
                    model=model,
                    provider=provider_label,
                    status="ok",
                    ttft_s=None if first_token_at is None else (first_token_at - started),
                    ttc_s=ended - started,
                    output_chars=chars,
                    error=None,
                ), "".join(chunks)
            return BenchResult(
                target="",
                model=model,
                provider=provider_label,
                status="fail",
                ttft_s=None if first_token_at is None else (first_token_at - started),
                ttc_s=ended - started,
                output_chars=chars,
                error="stream ended without done/error event",
            ), "".join(chunks)
    except urllib.error.HTTPError as err:
        detail = err.read().decode("utf-8", errors="replace")
        return BenchResult(
            target="",
            model=model,
            provider=provider_label,
            status="fail",
            ttft_s=None,
            ttc_s=None,
            output_chars=0,
            error=f"http {err.code}: {detail}",
        ), "".join(chunks)
    except Exception as err:  # noqa: BLE001
        return BenchResult(
            target="",
            model=model,
            provider=provider_label,
            status="fail",
            ttft_s=None,
            ttc_s=None,
            output_chars=0,
            error=str(err),
        ), "".join(chunks)


OUR_FIB_TESTS = """import unittest
import fib


class FibonacciReferenceTests(unittest.TestCase):
    def test_known_values(self):
        cases = {
            0: 0,
            1: 1,
            2: 1,
            3: 2,
            5: 5,
            8: 21,
            10: 55,
            20: 6765,
        }
        for n, expected in cases.items():
            self.assertEqual(fib.fibonacci(n), expected, f"n={n}")

    def test_negative_raises(self):
        with self.assertRaises(ValueError):
            fib.fibonacci(-1)


if __name__ == "__main__":
    unittest.main()
"""


def extract_fibonacci_implementation(text: str) -> Optional[str]:
    blocks = re.findall(r"```(?:python)?\s*\n(.*?)```", text, flags=re.IGNORECASE | re.DOTALL)
    for block in blocks:
        if "def fibonacci" in block:
            return block.strip() + "\n"
    if "def fibonacci" in text:
        lines = text.splitlines()
        start = None
        for idx, line in enumerate(lines):
            if line.strip().startswith("def fibonacci"):
                start = idx
                break
        if start is not None:
            snippet = "\n".join(lines[start:])
            return snippet.strip() + "\n"
    return None


def truncate_text(text: str, limit: int = 400) -> str:
    if len(text) <= limit:
        return text
    return text[:limit] + "..."


def sanitize_error_text(text: Optional[str]) -> Optional[str]:
    if not text:
        return text
    out = text
    # Redact obvious bearer/API-key values in provider payload echoes.
    out = re.sub(r"(?i)(bearer\\s+)([A-Za-z0-9._\\-]+)", r"\\1***", out)
    out = re.sub(
        r"(?i)(incorrect api key provided:\\s*)([^\\s\"',}]+)",
        r"\\1***",
        out,
    )
    out = re.sub(
        r"(?i)(api[_\\s-]*key\\s*[=:]\\s*)([^\\s\"',}]+)",
        r"\\1***",
        out,
    )
    # Redact common key prefixes.
    out = re.sub(r"\\b(sk|rk|pk|key)-[A-Za-z0-9_\\-]{8,}\\b", "***", out)
    return out


def is_auth_error(text: Optional[str]) -> bool:
    if not text:
        return False
    lowered = text.lower()
    markers = [
        "401",
        "403",
        "unauthorized",
        "authentication failed",
        "invalid_api_key",
        "wrong_api_key",
        "incorrect api key",
        "api key",
    ]
    if not any(marker in lowered for marker in markers):
        return False
    return ("unauthorized" in lowered) or ("api key" in lowered) or ("invalid" in lowered)


def normalize_result_for_reporting(target: Target, result: BenchResult) -> BenchResult:
    if result.error:
        result.error = sanitize_error_text(result.error)
    if result.status == "fail" and is_auth_error(result.error):
        result.status = "skip"
        result.correctness = "skip"
        result.error = (
            f"authentication failed; verify {'/'.join(target.key_envs)} "
            f"for provider `{result.provider}`"
        )
    return result


def is_model_not_found_error(error_text: Optional[str]) -> bool:
    if not error_text:
        return False
    lowered = error_text.lower()
    return (
        "model_not_found" in lowered
        or "model not found" in lowered
        or ("does not exist or you do not have access" in lowered and "model" in lowered)
    )


def cerebras_fallback_models(original_model: str) -> List[str]:
    env_value = _env_nonempty("BENCH_CEREBRAS_FALLBACK_MODELS")
    if env_value:
        candidates = [part.strip() for part in env_value.split(",") if part.strip()]
    else:
        candidates = ["gpt-oss-120b", "llama3.1-8b"]
    ordered: List[str] = []
    seen = {original_model}
    for model in candidates:
        if model in seen:
            continue
        seen.add(model)
        ordered.append(model)
    return ordered


def evaluate_fibonacci_correctness(response_text: str, timeout_s: int) -> Tuple[bool, str]:
    implementation = extract_fibonacci_implementation(response_text)
    if not implementation:
        return False, "could not extract a python fibonacci implementation from response"

    with tempfile.TemporaryDirectory(prefix="aish_fib_bench_") as tmp:
        root = Path(tmp)
        (root / "fib.py").write_text(implementation, encoding="utf-8")
        (root / "test_fib_reference.py").write_text(OUR_FIB_TESTS, encoding="utf-8")
        try:
            proc = subprocess.run(
                ["python3", "-m", "unittest", "-q", "test_fib_reference.py"],
                cwd=str(root),
                capture_output=True,
                text=True,
                timeout=max(2, timeout_s),
            )
        except subprocess.TimeoutExpired:
            return False, f"correctness test timed out after {timeout_s}s"

        if proc.returncode == 0:
            return True, "reference tests passed"

        merged = (proc.stdout or "") + ("\n" if proc.stdout and proc.stderr else "") + (proc.stderr or "")
        return False, truncate_text(merged.strip() or "unittest failed")


def _fmt(value: Optional[float]) -> str:
    return "-" if value is None else f"{value:.3f}"


def _truncate_cell(text: str, max_len: int) -> str:
    if max_len <= 0:
        return ""
    if len(text) <= max_len:
        return text
    if max_len <= 3:
        return "." * max_len
    return text[: max_len - 3] + "..."


def print_table(rows: List[BenchResult]) -> None:
    headers = [
        "provider",
        "model",
        "status",
        "correctness",
        "ttft_s",
        "ttc_s",
        "output_chars",
        "error",
    ]
    terminal_cols = shutil.get_terminal_size(fallback=(120, 24)).columns

    provider_cap = 16
    model_cap = 34
    status_cap = 8
    correctness_cap = 11
    ttft_cap = 7
    ttc_cap = 7
    output_chars_cap = 12

    values: List[List[str]] = []
    for r in rows:
        values.append(
            [
                _truncate_cell(r.provider, provider_cap),
                _truncate_cell(r.model, model_cap),
                _truncate_cell(r.status, status_cap),
                _truncate_cell(r.correctness, correctness_cap),
                _fmt(r.ttft_s),
                _fmt(r.ttc_s),
                str(r.output_chars),
                r.error or "",
            ]
        )

    fixed_widths = [len(h) for h in headers[:-1]]
    for row in values:
        for i, value in enumerate(row[:-1]):
            fixed_widths[i] = max(fixed_widths[i], len(value))

    fixed_total = sum(fixed_widths)
    separator_total = 2 * (len(headers) - 1)
    error_width = max(24, terminal_cols - fixed_total - separator_total)
    error_width = min(error_width, 160)

    for row in values:
        row[-1] = _truncate_cell(row[-1], error_width)

    widths = fixed_widths + [len(headers[-1])]
    for row in values:
        widths[-1] = max(widths[-1], len(row[-1]))

    def render(parts: List[str]) -> str:
        return "  ".join(part.ljust(widths[i]) for i, part in enumerate(parts))

    print(render(headers))
    print(render(["-" * w for w in widths]))
    for row in values:
        print(render(row))


def default_config_path() -> Path:
    env_path = _env_nonempty("AISH_CONFIG")
    if env_path:
        return Path(env_path).expanduser()
    return Path("~/.config/aish/aish.json").expanduser()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Benchmark TTFT/TTC for Fibonacci prompt via aishd provider routing."
    )
    parser.add_argument(
        "--aishd-url",
        default=_env_nonempty("AISHD_URL") or "http://127.0.0.1:5033",
        help="aishd base URL (default: $AISHD_URL or http://127.0.0.1:5033)",
    )
    parser.add_argument(
        "--config",
        default=str(default_config_path()),
        help="aish config path used to resolve provider profile names",
    )
    parser.add_argument(
        "--max-tokens",
        type=int,
        default=700,
        help="max_tokens sent to aishd (default: 700)",
    )
    parser.add_argument(
        "--json-out",
        default="",
        help="optional path to write JSON results",
    )
    parser.add_argument(
        "--no-correctness",
        action="store_true",
        help="disable correctness evaluation using built-in fibonacci reference tests",
    )
    parser.add_argument(
        "--correctness-timeout-s",
        type=int,
        default=15,
        help="timeout in seconds for local correctness unittest run (default: 15)",
    )
    parser.add_argument(
        "--temp-fallback-missing-profiles",
        action="store_true",
        help=(
            "when provider key exists but profile is missing, run via a temporary generated aishd profile"
        ),
    )
    args = parser.parse_args()

    health_err = check_health(args.aishd_url)
    if health_err:
        if not args.temp_fallback_missing_profiles:
            print(f"aishd health check failed at {args.aishd_url}: {health_err}", file=sys.stderr)
            print(
                "tip: pass --temp-fallback-missing-profiles to allow temporary provider fallback",
                file=sys.stderr,
            )
            return 1
        print(
            f"warning: aishd health check failed at {args.aishd_url}: {health_err} "
            "(temporary fallback daemons enabled)",
            file=sys.stderr,
        )

    cfg = _load_config(Path(args.config).expanduser())
    rows: List[BenchResult] = []

    for target in TARGETS:
        model = _model_for(target)
        api_key = _first_env(target.key_envs)
        if not api_key:
            rows.append(
                BenchResult(
                    target=target.key,
                    model=model,
                    provider="-",
                    status="skip",
                    ttft_s=None,
                    ttc_s=None,
                    output_chars=0,
                    error=f"missing API key ({'/'.join(target.key_envs)})",
                )
            )
            continue

        provider = resolve_provider_name(target, cfg)
        if provider is None:
            if args.temp_fallback_missing_profiles:
                result, response_text = _run_with_temp_aishd_for_target(
                    target=target,
                    model=model,
                    max_tokens=args.max_tokens,
                )
                if result.status == "skip" and not result.error:
                    result.error = (
                        "no matching aishd provider profile in config; "
                        f"set {target.provider_env}=<profile> or configure providers.openai_compat_profiles"
                    )
            else:
                result = BenchResult(
                    target=target.key,
                    model=model,
                    provider="-",
                    status="skip",
                    ttft_s=None,
                    ttc_s=None,
                    output_chars=0,
                    error=(
                        "no matching aishd provider profile in config; "
                        f"set {target.provider_env}=<profile> or configure providers.openai_compat_profiles "
                        "(or pass --temp-fallback-missing-profiles)"
                    ),
                )
                response_text = ""
        else:
            result, response_text = run_stream_benchmark(
                aishd_url=args.aishd_url,
                provider_name=provider,
                model=model,
                max_tokens=args.max_tokens,
            )
            result.provider = provider if provider else "default"
            if (
                target.key == "cerebras"
                and result.status == "fail"
                and is_model_not_found_error(result.error)
            ):
                for fallback_model in cerebras_fallback_models(model):
                    retry_result, retry_text = run_stream_benchmark(
                        aishd_url=args.aishd_url,
                        provider_name=provider,
                        model=fallback_model,
                        max_tokens=args.max_tokens,
                    )
                    retry_result.provider = provider if provider else "default"
                    result = retry_result
                    response_text = retry_text
                    if result.status == "ok" or not is_model_not_found_error(result.error):
                        break
        result.target = target.key
        if result.status == "ok" and not args.no_correctness:
            ok, detail = evaluate_fibonacci_correctness(
                response_text=response_text,
                timeout_s=args.correctness_timeout_s,
            )
            result.correctness = "pass" if ok else "fail"
            result.correctness_detail = detail
            if not ok and not result.error:
                result.error = f"correctness: {detail}"
        result = normalize_result_for_reporting(target, result)
        rows.append(result)
        print(
            f"[{result.status.upper()}] {target.display_name} provider={result.provider} "
            f"model={result.model} correctness={result.correctness} "
            f"ttft={_fmt(result.ttft_s)}s ttc={_fmt(result.ttc_s)}s chars={result.output_chars}"
        )
        if result.error:
            print(f"       error: {result.error}")

    print()
    print_table(rows)

    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as f:
            json.dump([asdict(r) for r in rows], f, indent=2)
        print(f"\nWrote JSON results to {args.json_out}")

    hard_fail = any(r.status == "fail" or r.correctness == "fail" for r in rows)
    any_ok = any(r.status == "ok" for r in rows)
    if hard_fail:
        return 2
    if not any_ok:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

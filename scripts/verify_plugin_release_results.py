"""Reject incomplete plugin distribution independently of core release status."""

import json
import os


def verify_results(results: dict[str, dict[str, str]]) -> int:
    """Every required runtime publishing job must run and succeed."""
    expected = {"publish_plugin_artifacts", "publish_plugin_npm", "publish_pypi_hermes", "publish_opencode"}
    if not isinstance(results, dict) or set(results) != expected:
        raise ValueError("Expected exactly four plugin distribution jobs")
    failed = [name for name, job in results.items() if job.get("result") != "success"]
    if failed:
        raise ValueError(f"Incomplete plugin distribution: {', '.join(failed)}")
    return len(expected)


if __name__ == "__main__":
    count = verify_results(json.loads(os.environ["RESULTS_JSON"]))
    print(f"Verified {count}/{count} plugin distribution jobs succeeded")

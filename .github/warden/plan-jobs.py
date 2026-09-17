#!/usr/bin/env python3
"""Emit the Warden job matrix: one job per (profile, skill).

Reads every profile under .github/warden/*.toml and pairs each of its
skills with the lane's runner facts below, so adding a skill to a profile
adds a job without touching the workflow. Printed as a GitHub Actions
output (`matrix=<json>`).

Splitting a lane into one job per skill gives each skill its own job
timeout and its own review, so a large PR yields the fast skills' results
even when a slow one is cut off. It does not raise a provider's ceiling:
the per-job `concurrency` below is chosen so the lane's jobs together stay
within what the provider (or the proxy's queue for it) tolerates.
"""

import glob
import json
import os
import sys
import tomllib

LANES = {
    # concurrency is per job; the lane runs len(skills) jobs at once, and
    # the proxy is the real scheduler: it holds 4 MiniMax slots and 5
    # third-party slots and answers a request that waits longer than its
    # queue timeout with a 503, which Warden's circuit breaker turns into a
    # failed skill after five in a row. So the budget is slots, not runners:
    #   minimax: two jobs at one — two slots for scanning, two left for the
    #     DeepSeek lane's verifier, which also goes through MiniMax (two jobs
    #     at two used all four and starved it; PR #144).
    #   thirdparty-deepseek: two jobs at one; the endpoint 429s above two.
    #   thirdparty-gpt-oss: eight jobs at one — eight on a five-slot backend
    #     shared with DeepSeek's long calls, which the queue absorbs; eight
    #     at two (sixteen) queued past the timeout and lost three skills.
    "minimax": {
        "provider": "MINIMAX",
        "app": "MINIMAX",
        "preflight-path": "/v1/models",
        "timeout": 300,
        "concurrency": 1,
    },
    "thirdparty-gpt-oss": {
        "provider": "THIRDPARTY",
        "app": "GPT_OSS",
        "preflight-path": "/models",
        "timeout": 90,
        "concurrency": 1,
    },
    "thirdparty-deepseek": {
        "provider": "THIRDPARTY",
        "app": "DEEPSEEK",
        "preflight-path": "/models",
        "timeout": 240,
        "concurrency": 1,
    },
}


def main() -> int:
    here = os.path.dirname(os.path.abspath(__file__))
    include = []
    for path in sorted(glob.glob(os.path.join(here, "*.toml"))):
        profile = os.path.splitext(os.path.basename(path))[0]
        lane = LANES.get(profile)
        if lane is None:
            print(f"::error::no lane facts for profile {profile} in plan-jobs.py", file=sys.stderr)
            return 1
        with open(path, "rb") as fh:
            config = tomllib.load(fh)
        for skill in config.get("skills", []):
            include.append({"profile": profile, "skill": skill["name"], **lane})
    if not include:
        print("::error::no skills found in any profile", file=sys.stderr)
        return 1
    print("matrix=" + json.dumps({"include": include}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

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
    # concurrency is per job; the lane runs len(skills) jobs at once.
    #   minimax: the proxy queues MiniMax at four in flight; two jobs at two.
    #   thirdparty-deepseek: the endpoint 429s above two in flight; two jobs at one.
    #   thirdparty-gpt-oss: no plan quota; eight jobs at two is the experiment's
    #     variable (the single job ran eight in flight in total).
    "minimax": {
        "provider": "MINIMAX",
        "app": "MINIMAX",
        "preflight-path": "/v1/models",
        "timeout": 300,
        "concurrency": 2,
    },
    "thirdparty-gpt-oss": {
        "provider": "THIRDPARTY",
        "app": "GPT_OSS",
        "preflight-path": "/models",
        "timeout": 90,
        "concurrency": 2,
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

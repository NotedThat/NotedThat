#!/usr/bin/env python3
"""Reduce a Warden profile to a single skill at a given hunk concurrency.

    one-skill.py <profile.toml> <skill-name> <concurrency>

Writes the reduced profile to stdout. Everything before the first
`[[skills]]` table (the shared review settings, models, runner) is kept,
with `concurrency = N` under `[runner]` replaced by the given value; of the
`[[skills]]` tables only the named one survives. The file is handled as
text rather than re-serialised so the profile's comments survive into the
run and the TOML the action reads is what a reviewer sees in git, minus
the other skills.
"""

import re
import sys
import tomllib


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    path, wanted, concurrency = sys.argv[1], sys.argv[2], int(sys.argv[3])
    text = open(path, encoding="utf-8").read()

    parts = re.split(r"(?m)^(?=\[\[skills\]\]\n)", text)
    head, blocks = parts[0], parts[1:]
    kept = [b for b in blocks if re.search(rf'(?m)^name = "{re.escape(wanted)}"\s*$', b)]
    if len(kept) != 1:
        print(f"::error::skill {wanted!r} appears {len(kept)} times in {path}", file=sys.stderr)
        return 1

    head, n = re.subn(r"(?m)^(\[runner\]\n(?:#[^\n]*\n)*concurrency = )\d+", rf"\g<1>{concurrency}", head)
    if n != 1:
        print(f"::error::could not find `[runner]` / `concurrency = N` in {path}", file=sys.stderr)
        return 1

    out = head + kept[0]
    # A skill's comment lines sit above its `[[skills]]` line and so end up
    # in the preceding block; that is cosmetic. What matters is that the
    # result parses and carries exactly the one skill.
    parsed = tomllib.loads(out)
    names = [s["name"] for s in parsed.get("skills", [])]
    if names != [wanted] or parsed["runner"]["concurrency"] != concurrency:
        print(f"::error::reduced profile is not as expected: skills={names}", file=sys.stderr)
        return 1
    sys.stdout.write(out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

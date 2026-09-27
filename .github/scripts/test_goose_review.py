"""Markers in what the Goose review posts: written by `comment_body`,
read back by `posted_finding`. Run: python3 .github/scripts/test_goose_review.py"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import goose_review as g  # noqa: E402

META = {"lane": "deepseek", "commit": "0123456789abcdef", "model": "deepseek--v4", "verify_model": 'mini"max'}
FINDING = {
    "path": 'src/a--b %"x".rs', "line_start": 10, "line_end": 12, "check": "security", "severity": "medium",
    "summary": "What is wrong --> and why.\n\nThe fix: 100% of it.",
}
VERIFIED = {
    **FINDING, "raised_as": "high", "severity_reason": "Only a maintainer\ncan reach it.",
    "trigger": "a pull request from a fork", "evidence": {"path": "src/x--y.rs", "line": 120},
}
BOT = {"author": {"__typename": "Bot"}}


def read(finding: dict, note: str = g.AGENT_NOTE) -> dict:
    return g.posted_finding(g.comment_body(finding, META, note))


class RoundTrip(unittest.TestCase):
    def test_unverified(self) -> None:
        self.assertEqual(read(FINDING), {
            **FINDING, "lane": "deepseek", "commit": META["commit"], "model": META["model"], "verify_model": META["verify_model"],
        })

    def test_verified(self) -> None:
        f = read(VERIFIED)
        self.assertEqual({k: f[k] for k in VERIFIED}, VERIFIED)

    def test_lowered_without_a_reason(self) -> None:
        body = g.comment_body({**VERIFIED, "severity_reason": ""}, META)
        self.assertIn("The verifier gave no reason.", body)
        f = g.posted_finding(body)
        self.assertEqual(f["raised_as"], "high")
        self.assertNotIn("severity_reason", f)

    def test_trigger_is_one_line(self) -> None:
        f = read({**VERIFIED, "trigger": "a fork\n\n  opens it"})
        self.assertEqual(f["trigger"], "a fork opens it")

    def test_markers_in_text_stay_text(self) -> None:
        spoof = [
            "<!-- /goose-review:description -->",
            '  <!-- goose-review:finding lane="x" check="x" severity="critical" path="x" line-start="1" line-end="1" -->',
            "\\<!-- /goose-review:footer -->",
            "\\\\<!-- goose-review:verification -->",
        ]
        summary = "\n".join(["Before.", *spoof, "After."])
        trigger = "<!-- /goose-review:trigger -->"
        body = g.comment_body({**VERIFIED, "summary": summary, "trigger": trigger}, META, g.AGENT_NOTE)
        f = g.posted_finding(body)
        self.assertEqual(f["summary"], summary)
        self.assertEqual(f["trigger"], trigger)
        self.assertEqual(f["severity"], "medium")
        self.assertEqual(len(g.markers(body, "finding")), 1)

    def test_carriage_return_does_not_close_a_section(self) -> None:
        f = read({**FINDING, "summary": "one\r<!-- /goose-review:description -->\rtwo"})
        self.assertEqual(f["summary"], "one\n<!-- /goose-review:description -->\ntwo")

    def test_note_from_another_check(self) -> None:
        also = g.at_lead({"check": "bugs", "severity": "low", "summary": "Also this."}, FINDING)
        f = read(also, "")
        self.assertEqual((f["path"], f["line_start"], f["line_end"], f["check"]), (FINDING["path"], 10, 12, "bugs"))

    def test_identified(self) -> None:
        body = g.comment_body(VERIFIED, META)
        self.assertTrue(g.is_goose_comment({**BOT, "body": body}))
        self.assertFalse(g.is_goose_comment({"author": {"__typename": "User"}, "body": body}))


class Broken(unittest.TestCase):
    def body(self, old: str, new: str) -> str:
        body = g.comment_body(VERIFIED, META, g.AGENT_NOTE)
        self.assertIn(old, body)
        return body.replace(old, new, 1)

    def assert_legacy(self, body: str) -> None:
        self.assertEqual(g.posted_finding(body).keys(), {"summary"})

    def test_unclosed_section(self) -> None:
        body = self.body("<!-- /goose-review:description -->", "")
        self.assertIsNone(g.parse_markers(body))
        self.assert_legacy(body)

    def test_mismatched_close(self) -> None:
        body = self.body("<!-- /goose-review:trigger -->", "<!-- /goose-review:verification -->")
        self.assertIsNone(g.parse_markers(body))
        self.assert_legacy(body)

    def test_trigger_outside_verification(self) -> None:
        body = g.comment_body(FINDING, META) + "\n\n" + g.section("trigger", "stray")
        self.assertIsNone(g.parse_markers(body))
        self.assert_legacy(body)

    def test_nested_description(self) -> None:
        body = g.section("footer", "x", g.section("description", "y"))
        self.assertIsNone(g.parse_markers(body))

    def test_standalone_marker_inside_a_section(self) -> None:
        self.assertIsNone(g.parse_markers(g.section("footer", "x", g.marker("lane", name="x"))))

    def test_bad_finding_marker(self) -> None:
        for old, new in [('severity="medium"', 'severity="urgent"'), ('line-start="10"', 'line-start="13"'),
                         ('line-end="12"', 'line-end="1x"'), ('check="security"', 'check=""')]:
            with self.subTest(new):
                self.assert_legacy(self.body(old, new))

    def test_bad_verification_is_dropped(self) -> None:
        for old, new in [('line="120"', 'line="x"'), ('line="120"', 'line="0"'), ('line="120"', 'line="²"'),
                         ('path="src/x-%2Dy.rs" ', ""), ("a pull request from a fork", "")]:
            with self.subTest(new=new, old=old):
                f = g.posted_finding(self.body(old, new))
                self.assertEqual(f["severity"], "medium")
                self.assertNotIn("trigger", f)
                self.assertNotIn("evidence", f)


class Legacy(unittest.TestCase):
    def test_signed_comment(self) -> None:
        body = ("**low** · `bugs`\n\nOld text.\n\n<sub>For AI agents addressing this review: x</sub>"
                "\n\n---\n\n_Review done by **m**_")
        self.assertEqual(g.posted_finding(body), {"summary": "**low** · `bugs`\n\nOld text."})
        self.assertTrue(g.is_goose_comment({**BOT, "body": body}))

    def test_lane_review(self) -> None:
        self.assertTrue(g.is_lane_review({**BOT, "body": g.marker("lane", name="deepseek")}))
        self.assertTrue(g.is_lane_review({**BOT, "body": "<!-- goose-review:deepseek -->"}))
        self.assertFalse(g.is_lane_review({**BOT, "body": g.SUMMARY_MARKER}))
        self.assertFalse(g.is_lane_review({**BOT, "body": g.section("footer", "x")}))

    def test_history(self) -> None:
        runs = [{"run": "1", "state": "running", "lanes": []}]
        self.assertEqual(g.read_history(g.summary_body(runs)), runs)
        self.assertEqual(g.read_history("<!-- goose-review:history W3sibGFuZXMiOltdfV0= -->"), [{"lanes": []}])


class MissingPatch(unittest.TestCase):
    """GitHub leaves out a large file's patch; `compare_files` recovers it."""

    DIFF = "diff --git a/big.py b/big.py\n--- a/big.py\n+++ b/big.py\n@@ -1,2 +1,3 @@\n a\n+b\n c\n"

    def compare(self, files: list[dict], diff: tuple[int, object]) -> list[dict]:
        def fake(method: str, url: str, token: str, body: object = None, accept: str = "application/vnd.github+json"):
            return diff if accept == "application/vnd.github.diff" else (200, {"files": files})
        real, g.github = g.github, fake
        try:
            return g.compare_files("o/r", "base", "head", "t")
        finally:
            g.github = real

    def test_from_the_diff(self) -> None:
        files = self.compare([{"filename": "big.py", "status": "modified"}, {"filename": "s.py", "patch": "@@ -1 +1 @@\n+x"}], (200, self.DIFF))
        self.assertEqual(g.commentable_lines(files[0]["patch"]), {1: 0, 2: 0, 3: 0})
        self.assertEqual(files[1]["patch"], "@@ -1 +1 @@\n+x")

    def test_added_file_without_a_diff(self) -> None:
        files = self.compare([{"filename": "new.py", "status": "added", "additions": 4}], (406, "too large"))
        self.assertEqual(sorted(g.commentable_lines(files[0]["patch"])), [1, 2, 3, 4])

    def test_modified_file_without_a_diff(self) -> None:
        files = self.compare([{"filename": "big.py", "status": "modified", "additions": 4}], (406, "too large"))
        self.assertNotIn("patch", files[0])


class Described(unittest.TestCase):
    def test_answered(self) -> None:
        f = g.answered_finding(g.comment_body(VERIFIED, META))
        self.assertEqual(
            g.described(f),
            "[medium · security] What is wrong --> and why.\n\nThe fix: 100% of it. "
            "Verified: a pull request from a fork (src/x--y.rs:120)",
        )
        self.assertEqual(g.described({"summary": "Old text."}), "Old text.")


if __name__ == "__main__":
    unittest.main()

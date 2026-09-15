# pi model catalogue for Warden profiles

Warden's default runtime is [pi](https://github.com/badlogic/pi-mono), which
knows the big providers out of the box. A provider it does not know is
declared here in pi's `models.json` format; the workflow copies this
directory to a scratch location and points `PI_CODING_AGENT_DIR` at it.

`thirdparty` is an OpenAI-compatible endpoint we have access to, reached
through our egress proxy. Neither location is in the repository: the
`baseUrl` here is a placeholder that the workflow replaces with the
`WARDEN_THIRDPARTY_BASE_URL` secret (the proxy route) at run time, and
`WARDEN_THIRDPARTY_API_KEY` is the proxy's token, which Warden mirrors to
the `$THIRDPARTY_API_KEY` this file references. The proxy holds the real
provider key.

Two models are listed, each with its own profile overlay. In the first
bake-off `openai/gpt-oss-120b` was the only model on that endpoint that
investigated: on a 15-hunk PR it made 78 tool calls (reads, greps) before
answering, while the others returned empty responses, never used tools, or
produced a confident hallucinated finding in two seconds. That turned out
to be the endpoint's gateway, not the models: it forwards
`tool_choice: "none"` whenever a client omits the field, pi never sends it,
and GPT-OSS was the one model whose vLLM parser ignores `tool_choice`. The
proxy now inserts `tool_choice: "auto"` on this route, and through it
`deepseek-v4-flash-0731` investigated properly on PR #138 (up to 16
tool-calling turns per hunk, zero false positives on a PR where the other
lanes reported two), so it has its own lane. The endpoint rate-limits that
model at more than two hunks in flight; the overlay sets the concurrency.
Re-test the remaining chat models through the proxy before adding one.

`compat.supportsDeveloperRole` is off because the endpoint is a vLLM
deployment that wants the system prompt as `system`. `supportsReasoningEffort`
is on: GPT-OSS is a reasoning model and the endpoint accepts
`reasoning_effort` low|medium|high (not xhigh/max); the profile overlay sets
`effort = "high"` on both lanes.

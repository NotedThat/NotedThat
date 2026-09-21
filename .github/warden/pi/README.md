# pi model catalogue for Warden profiles

Warden's default runtime is [pi](https://github.com/badlogic/pi-mono), which
knows the big providers out of the box. A provider it does not know is
declared here in pi's `models.json` format; the workflow copies this
directory to a scratch location and points `PI_CODING_AGENT_DIR` at it.

Two providers are configured, both reached through our egress proxy:

- `minimax` is pi's built-in provider; the entry here only replaces its
  base URL with the proxy's Anthropic-style route (pi merges such an entry
  into the built-in provider and keeps its models and auth handling).
- `thirdparty` is an OpenAI-compatible endpoint pi does not know, declared
  in full.

Neither route is in the repository: the `baseUrl` values here are
placeholders that the workflow replaces with the `WARDEN_MINIMAX_BASE_URL`
and `WARDEN_THIRDPARTY_BASE_URL` secrets at run time. This is done through
the file rather than Warden's `WARDEN_<PROVIDER>_BASE_URL` override because
the override is not honoured inside the v0.48.0 action (the CLI does honour
it; both lanes hit this in CI, see the workflow). The keys are the proxy's
token: `WARDEN_MINIMAX_API_KEY` and `WARDEN_THIRDPARTY_API_KEY`, which
Warden mirrors to the env names pi expects (`$THIRDPARTY_API_KEY` is what
this file references). The proxy holds the real provider keys.

Two third-party models are listed, each with its own profile overlay — though
only the DeepSeek one runs today: the GPT-OSS lane is switched off in the
workflow because its chat route kept failing with a gateway-side error (see
`thirdparty-gpt-oss.toml`); its entry stays here so it can be re-enabled
without re-declaring the model. In the first
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

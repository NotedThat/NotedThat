# pi model catalogue for Warden profiles

Warden's default runtime is [pi](https://github.com/badlogic/pi-mono), which
knows the big providers out of the box. A provider it does not know is
declared here in pi's `models.json` format; the workflow copies this
directory to a scratch location and points `PI_CODING_AGENT_DIR` at it.

`thirdparty` is an OpenAI-compatible endpoint we have access to. Its real
location is deliberately not in the repository: the `baseUrl` here is a
placeholder that Warden replaces from the `WARDEN_THIRDPARTY_BASE_URL`
secret at run time (`WARDEN_<PROVIDER>_BASE_URL` is a documented override),
and the key comes from `WARDEN_THIRDPARTY_API_KEY`, which Warden mirrors to
the `$THIRDPARTY_API_KEY` this file references.

Only `openai/gpt-oss-120b` is listed because it is the only model on that
endpoint that actually investigates: on a 15-hunk PR it made 78 tool calls
(reads, greps) before answering. The other chat models there either
returned empty responses to review prompts, never used tools, or produced a
confident hallucinated finding in two seconds. Re-test before adding one.

`compat.supportsDeveloperRole` / `supportsReasoningEffort` are off because
the endpoint is a vLLM deployment that answers the system prompt as
`system` and ignores `reasoning_effort`.

# Golutra Terminal Smoke

Generated: `2026-08-28T22:37:10.300Z`

This isolated live smoke used `gpt-5.5` over the OpenAI Responses protocol and
ran `python3 -m unittest discover -s tests -v` without changing the workspace.

| Metric | Result |
| --- | ---: |
| Objective test command | pass |
| Runtime terminal success | yes |
| Process return code | 0 |
| Strict four-stage status | not applicable |
| Provider requests | 2 |
| Tool calls | 1 |
| Prompt input | 1,254 |
| Uncached input | 1,254 |
| Cache read | 0 |
| Cache write | 0 |
| Output | 201 |
| Final message | `SMOKE_OK` |

The runtime now recognizes Python unittest's standard `Ran N tests` evidence.
Because this is a cold single-turn run, `cache read = 0` is expected and does
not measure same-session cache reuse.

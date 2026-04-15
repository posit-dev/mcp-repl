# Future Work: REPL Interaction Rough Edges

## Summary

Potential follow-on cleanup: tighten a few user-visible REPL interaction details that worked
functionally during live use but still felt rough enough to slow inspection and debugging.

These are not current contract bugs. They are candidate polish items observed during an
interactive R session that exercised timeouts, output bundles, interrupts, restarts, plots,
and `browser()`.

## Candidate follow-ups

### 1. Make interrupts acknowledge themselves clearly

Observed behavior:

- Sending `\u0003` stopped the running loop and returned the prompt.
- The visible reply only showed an empty `stderr:` line before the prompt.

Why this is worth revisiting:

- The interrupt appears to work, but the reply does not say so explicitly.
- A future agent or human cannot easily distinguish "interrupt succeeded" from "worker went
  quiet and happened to become idle."

Potential direction:

- Return one explicit interrupt status line when an interrupt is delivered successfully.
- Avoid emitting a blank `stderr:` section when there is no actual stderr payload.

### 2. Make transcript bundles faithfully capture the visible turn

Observed behavior:

- A text-only oversized-output bundle produced a correct `transcript.txt` with the expected
  total line count and tail content.
- The start of the transcript did not include an echoed prompt/input line for the triggering
  `writeLines(big_lines)` call, even though later prompt lines were present.

Why this is worth revisiting:

- The bundle is inspectable, but it does not read like a faithful REPL transcript.
- Missing prompt/input echo at the start of a turn makes bundle inspection weaker for
  debugging and post-hoc auditing.

Potential direction:

- Define whether bundle transcripts are intended to be full REPL transcripts or worker-output
  captures with selective prompt echo.
- If the intent is transcript fidelity, ensure the first accepted input for a bundled turn is
  recorded consistently with later echoed prompt lines.

### 3. Make busy/discarded-input status less ambiguous

Observed behavior:

- Sending non-empty input while a timed-out request was still running returned both:
  - `<<repl status: busy, write_stdin timeout reached; elapsed_ms=...>>`
  - `[repl] input discarded while worker busy`

Why this is worth revisiting:

- The discard notice is the important event for the second call.
- The timeout wording reads like the new input itself timed out, which is not what happened.

Potential direction:

- Split "worker still busy from an earlier turn" from "this call timed out while waiting."
- Prefer one status path for rejected/discarded concurrent input, without reusing timeout
  wording from the original running request.

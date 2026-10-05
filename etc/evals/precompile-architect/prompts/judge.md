You are grading one criterion of a plan written by an engineer for a change to blockchain precompiles. The plan must preserve execution consensus, meaning blocks that already executed must replay identically.

You grade exactly one criterion. Ignore every other quality of the plan.

## Ticket the engineer received

{ticket}

## Network upgrade status at that commit

{fork_table}

## Expected plan, written by the eval authors

```json
{expected}
```

## Grader notes

{notes}

## Criterion

{check}

## Plan to grade

<plan>
{plan}
</plan>

## Instructions

Decide whether the plan satisfies the criterion. The expected plan is one correct answer. Credit an equivalent approach that meets the criterion's intent.

- Answer `yes` or `no` whenever the plan takes a position on the criterion, even if the case is borderline.
- Answer `unknown` only when the plan does not address the criterion at all, so no position can be found. Do not use `unknown` to avoid a hard call.

Reply with only this JSON object:

{{"verdict": "yes" | "no" | "unknown", "evidence": "<verbatim quote from the plan, or empty>", "reason": "<one sentence>"}}

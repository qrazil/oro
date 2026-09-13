# Deferred ideas — revisit candidates, not commitments

This file holds ideas that are **cut for now on purpose**, with the trigger that
would reopen each one. An entry here is not a roadmap item and not a promise; it
is a decision recorded so it does not have to be re-argued from scratch, and so
that the thing that would change the decision is written down in advance.

The bar is the same as everywhere else in Oro: a new spelling has to earn its
place against the one that already works. Nothing here is added on speculation —
each waits for a real program to prove the current spelling genuinely hurts.

---

## `dig` — nested optional access

**Status:** deferred. Do not add on speculation.

**Problem it would solve.** Nested JSON access is verbose. Today you write:

```oro
name = data.get("user", default={}).get("profile", default={}).get("name")
```

which works and is explicit, but gets tedious at depth.

**Proposed shape.** `data.dig("user", "profile", "name")` — walk the keys/indices
in order, returning `null` if any link in the chain is missing. This is Ruby's
`Hash#dig`.

**Why `dig` and not JavaScript's `?.` operator, if it is added at all:**

- `?.` is a **new operator** with its own precedence and interaction rules;
  `dig` is a method that fits the current grammar with no syntax change.
- `a?.b?.c` returning `null` tells you **nothing about which link was missing** —
  it conflates "no `a`", "no `b`" and "`c` is genuinely null" into one answer.
  That is the same implicit-conflation bug class the grammar pass just removed by
  cutting truthiness (`x or default` conflating absent with empty, `if d.get(k)`
  conflating a missing key with a `0`/`null` value). Adding `?.` would reintroduce
  it under a different spelling.
- `?.` generalises to **attribute** access, where silent null-propagation is
  murkiest; `dig` stays scoped to **key/index lookup**, which is where the real
  pain is (JSON).
- `dig` would be a **second null-handling spelling** alongside `!= null` and
  `.get(k, default=…)`, so it has to earn its place, not merely be convenient.

**Revisit trigger.** After a real Oro script that handles nested JSON has been
written. If the `.get(k, default={})` chain proves genuinely painful in practice,
`dig` is the answer. If it does not come up, this stays cut.

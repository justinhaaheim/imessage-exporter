# Important guidelines

# Use beads for planning and tracking

I use [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) as the single source of truth for plans, tasks, and progress. **Do NOT write planning documents in `docs/plans/` or anywhere else.** Use beads.

## Why this matters

Context evaporates between sessions. Without externalized state, resuming work means rebuilding context from chat history (gone), file diffs (lossy), and memory (fragile). The fix: **always capture "what's next" at the END of a session, while context is fresh — not at the START of the next session, when it's gone.**

That means:

- Don't end a session with in-progress beads in vague states. Update their notes.
- Don't end without creating beads for the follow-ups you noticed during the work.
- Don't leave the project in a state where "what to work on next" requires reading code.

The hand-off moment (end of session, end of an epic phase, mid-task interrupt) is the most valuable place to apply this. Treat it as a first-class step, not an afterthought.

## When you start work

Before doing anything, decide which case you're in:

1. **Trivial task** (single file edit, simple fix) — create one bead, do the work, close it.

   ```bash
   br create "Short imperative title" -t task -p 2
   # ... do the work ...
   br close <id> --reason "..."
   ```

2. **Non-trivial task** (multiple files, real design decisions, multi-session work) — create an **epic**, plan it in the epic's description / design / acceptance-criteria / notes fields, then break it into **child beads**.

   ```bash
   br create "Short epic title" -t epic -p 2
   # Get back the epic id, e.g. voice-recorder-10c

   # Flesh out the epic — description (vision/context), design (technical
   # approach), acceptance_criteria (what "done" means), notes (open
   # questions, references). Use `br update` with the appropriate flags,
   # or pass them on create.

   br create "Phase 1 thing" --parent voice-recorder-10c
   br create "Phase 2 thing" --parent voice-recorder-10c
   # Sub-beads auto-number: voice-recorder-10c.1, voice-recorder-10c.2, ...
   ```

   Using `--parent` does two things at once: creates the sub-bead AND establishes the `parent-child` relationship correctly. Use this instead of `br dep add` for epic / sub-bead relationships.

3. **Already-planned work** — check `br ready` for the highest-priority unblocked bead and pick one up.

   ```bash
   br ready                              # what's actionable right now
   br epic status                        # how active epics are progressing
   br show <id>                          # full context for one bead
   br update <id> --status=in_progress   # claim it
   ```

## During work

- Update bead status as you go: `in_progress` when claimed, `closed` with a `--reason` when done.
- Discover a follow-up task? Create it: `br create "..." -t task -p N` (or with `--parent EPIC` if it belongs to an epic).
- Surprises, decisions, dead-ends worth remembering — append to the bead's `notes` field with `br update <id> --notes ...`. The bead is the durable memory; chat history is not.

## When you finish (or hit a stopping point)

Always end with these steps:

```bash
br sync --flush-only    # exports the DB to .beads/issues.jsonl (committed in git)
git add ...             # stage your changes (code + .beads/issues.jsonl)
git commit -m '...'     # commit with [Model Name] prefix (see COMMIT_REGULARLY)
```

If the work is a multi-bead epic and you're handing off mid-stream, give Justin a **short handoff message** he can paste to the next agent. Include: the epic id, the branch, what's done, what's next (which bead to claim).

## When you finish _planning_ an epic

If Justin asked you to just plan (not implement), report back:

- The epic id (e.g. `voice-recorder-10c`)
- Whether you wrote any sub-beads, and if so the count + a one-line summary of each
- A short "start the next thread" message — usually the epic id + which sub-bead to claim first + the branch name

## When Justin asks "what should I work on?"

- Run `br ready` to see open, unblocked beads ordered by priority.
- Run `br epic status` to see how active epics are progressing.
- Present concrete options with context — don't make him read raw command output. Help him pick something that moves toward a meaningful milestone.

## Notes

- If a project doesn't have beads set up yet, run `br init` in the repo root.
- The `.beads/` directory IS the source of truth, and the JSONL export is committed to git so beads state travels with the branch.
- Bead types: `task`, `bug`, `feature`, `epic`, `chore`, `docs`, `question`. Priorities are 0–4 (P0 = critical, P4 = backlog).
- Dependency directions matter — see the project's `AGENTS.md` for the full reference. The short version: use `--parent EPIC_ID` for epic / sub-bead; use `br dep add <waiter> <waited-on>` (default `--type blocks`) for true sequencing.
- Beads replaces both the old `ROADMAP.md` north-star doc and the `docs/plans/` scratchpad pattern. Don't reintroduce either. If you find yourself wanting to write a planning markdown file, write a bead instead.

# Make a plan

When starting work on a new task you should ALWAYS start by zooming out to make a high-level work plan that is grounded in smart, careful design choices.

- ALWAYS make this plan and share it with me explicitly in our chat before starting your work.
- Wait to begin coding until I've OK'ed your plan.

# Use beads for planning and tracking

I use [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) as the single source of truth for plans, tasks, and progress. **Do NOT write planning documents in `docs/plans/` or anywhere else.** Use beads.

## Why this matters

Context evaporates between sessions. Without externalized state, resuming work means rebuilding context from chat history (gone), file diffs (lossy), and memory (fragile). The fix: **always capture "what's next" at the END of a session, while context is fresh — not at the START of the next session, when it's gone.**

That means:

- Don't end a session with in-progress beads in vague states. Update their notes.
- Don't end without creating beads for the follow-ups you noticed during the work.
- Don't leave the project in a state where "what to work on next" requires reading code.

The hand-off moment (end of session, end of an epic phase, mid-task interrupt) is the most valuable place to apply this. Treat it as a first-class step, not an afterthought.

## When you start work

Before doing anything, decide which case you're in:

1. **Trivial task** (single file edit, simple fix) — create one bead, do the work, close it.

   ```bash
   br create "Short imperative title" -t task -p 2
   # ... do the work ...
   br close <id> --reason "..."
   ```

2. **Non-trivial task** (multiple files, real design decisions, multi-session work) — create an **epic**, plan it in the epic's description / design / acceptance-criteria / notes fields, then break it into **child beads**.

   ```bash
   br create "Short epic title" -t epic -p 2
   # Get back the epic id, e.g. voice-recorder-10c

   # Flesh out the epic — description (vision/context), design (technical
   # approach), acceptance_criteria (what "done" means), notes (open
   # questions, references). Use `br update` with the appropriate flags,
   # or pass them on create.

   br create "Phase 1 thing" --parent voice-recorder-10c
   br create "Phase 2 thing" --parent voice-recorder-10c
   # Sub-beads auto-number: voice-recorder-10c.1, voice-recorder-10c.2, ...
   ```

   Using `--parent` does two things at once: creates the sub-bead AND establishes the `parent-child` relationship correctly. Use this instead of `br dep add` for epic / sub-bead relationships.

3. **Already-planned work** — check `br ready` for the highest-priority unblocked bead and pick one up.

   ```bash
   br ready                              # what's actionable right now
   br epic status                        # how active epics are progressing
   br show <id>                          # full context for one bead
   br update <id> --status=in_progress   # claim it
   ```

## During work

- Update bead status as you go: `in_progress` when claimed, `closed` with a `--reason` when done.
- Discover a follow-up task? Create it: `br create "..." -t task -p N` (or with `--parent EPIC` if it belongs to an epic).
- Surprises, decisions, dead-ends worth remembering — append to the bead's `notes` field with `br update <id> --notes ...`. The bead is the durable memory; chat history is not.

## When you finish (or hit a stopping point)

Always end with these steps:

```bash
br sync --flush-only    # exports the DB to .beads/issues.jsonl (committed in git)
git add ...             # stage your changes (code + .beads/issues.jsonl)
git commit -m '...'     # commit with [Model Name] prefix (see COMMIT_REGULARLY)
```

If the work is a multi-bead epic and you're handing off mid-stream, give Justin a **short handoff message** he can paste to the next agent. Include: the epic id, the branch, what's done, what's next (which bead to claim).

## When you finish _planning_ an epic

If Justin asked you to just plan (not implement), report back:

- The epic id (e.g. `voice-recorder-10c`)
- Whether you wrote any sub-beads, and if so the count + a one-line summary of each
- A short "start the next thread" message — usually the epic id + which sub-bead to claim first + the branch name

## When Justin asks "what should I work on?"

- Run `br ready` to see open, unblocked beads ordered by priority.
- Run `br epic status` to see how active epics are progressing.
- Present concrete options with context — don't make him read raw command output. Help him pick something that moves toward a meaningful milestone.

## Notes

- If a project doesn't have beads set up yet, run `br init` in the repo root.
- The `.beads/` directory IS the source of truth, and the JSONL export is committed to git so beads state travels with the branch.
- Bead types: `task`, `bug`, `feature`, `epic`, `chore`, `docs`, `question`. Priorities are 0–4 (P0 = critical, P4 = backlog).
- Dependency directions matter — see the project's `AGENTS.md` for the full reference. The short version: use `--parent EPIC_ID` for epic / sub-bead; use `br dep add <waiter> <waited-on>` (default `--type blocks`) for true sequencing.
- Beads replaces both the old `ROADMAP.md` north-star doc and the `docs/plans/` scratchpad pattern. Don't reintroduce either. If you find yourself wanting to write a planning markdown file, write a bead instead.

When making your plan put on your "Savvy, discerning senior engineer hat":

- Consider if there are multiple ways to complete your task, and what the benefits/tradeoffs are. If you see a simpler/better way to do something, tell me!
- ALWAYS prioritize the approaches that...
  - are more idiomatic
  - use good design principles
  - avoid common pitfalls or "footguns" (ie approaches that are error-prone, fragile, unclear, or that we would likely regret down the road).

# Stay focused

- Focus on addressing the task I’ve given you in the smartest, most direct way possible.
- Always prioritize getting your change _working_ over fixing lint/typescript issues that arise. Return to fix the lint/ts issues at the end.
- Do not change anything that is not directly related to the task you are working on. Do not alter/remove comments or code unless it is required for your task, or are explicitly instructed to.

# Use good style

- **NEVER disable a lint rule unless explicitly authorized to do so.**
  - The lint rules for this project were carefully chosen for a reason. These rules help prevent anti-patterns, mistakes, and hard-to-debug code.
  - You should focus on getting your change WORKING first, but always come back and address lint/ts issues
  - You should always attempt to _improve the code_ in order to address the warnings/errors.
  - If you get stuck addressing a lint/ts issue you can move onto the next one, but ALWAYS explicitly flag the issue to me if you needed to skip over it.

- Use `null` when a particular property is absent (instead of an empty string, empty array, the number 0, etc.). For instance, if an object has a property `uri` that is not known now, but will be known in the future, then `uri` should be set to `null` instead of an empty string. This improves clarity and reduces bugs. Note that you may need to update the typescript type definition, and accommodate the new null possibility at other points in the code.

- When creating a new file/component check the codebase for code that already exists to fulfill that purpose. If you are uncertain whether to change existing code or create new code, ask me in our chat.

- Use functional, declarative programming. Never create javascript classes unless specifically requested. Prefer the module pattern over classes.

- Use the function declaration syntax for functions/components at the top level of a file. Otherwise use whatever is most idiomatic.

## React:

- Use functional components
- Rigorously follow good react patterns, and avoid anti-patterns
- Avoid patterns that cause React to re-render needlessly
- Memoize callbacks/objects with `useCallback` or `useMemo`
- NEVER NEVER NEVER disable the `exhaustive-deps` lint rule that applies to `useEffect`, `useCallback`, `useMemo`, etc. This is an anti-pattern, and is very likely to introduce bugs that are hard to detect.
  - Instead write explicit logic. If an effect should only run once, for example, check `if (didRunRef.current) return;` at the start of the effect.
- Use `useEffect` to _synchronize a component with an external system_. Use it for code that should run _because_ the component was displayed ot the user.
  - In most other cases you should be handling things imperatively as part of an event handler

# Check your work

Check your work with `npm run signal`:

- `npm run signal` - Check for typescript, lint and formatting issues all at once

You can also use these scripts to directly check for issues:

- `npm run ts-check` - Check for typescript errors
- `npm run lint` - Check for lint issues

You can also use the tools directly via `npx`:

- `npx tsc --noEmit [file]`
- `npx eslint [file]`

Do NOT use `cd [path] && [command]` in your commands unless it is absolutely necessary. `cd` commands are blocked by default and require explicit permission, which slows us both down.

# Commit regularly

- Commit your changes regularly using `git add ...` and `git commit ...` (run them as separate commands, not chained).
- Start all your commit messages with the model name and number in use in brackets (ie `[Claude 4 Sonnet] `, `[Gemini 2.5 Pro] `, etc).
- Before committing, run `br sync --flush-only` to export beads state to `.beads/issues.jsonl`, and include that file in the commit. Beads changes do NOT auto-commit — they only land in git if you stage the JSONL yourself.

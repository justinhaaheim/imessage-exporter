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

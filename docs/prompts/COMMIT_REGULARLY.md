# Commit regularly

- Commit your changes regularly using `git add ...` and `git commit ...` (run them as separate commands, not chained).
- Start all your commit messages with the model name and number in use in brackets (ie `[Claude 4 Sonnet] `, `[Gemini 2.5 Pro] `, etc).
- Before committing, run `br sync --flush-only` to export beads state to `.beads/issues.jsonl`, and include that file in the commit. Beads changes do NOT auto-commit — they only land in git if you stage the JSONL yourself.

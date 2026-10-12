# Project Progress


## Current State
- Latest commit: d6ec37b (feat: implement the jAI agent with CLI and web mode)

## Completed
- Commanline CLI without TUI.
- Check tool "filesystem_write" if a edit tool is needed: yes — added `filesystem_edit`,
  which replaces an exact snippet (anchored, `expected_replacements`, default 1)
  instead of rewriting the whole file. Wired through the policy engine as a
  **write** (never auto-approved by `auto_approve_read_only`), documented in
  `docs/tools.md`, and covered by tests in `jai-tools` and `jai-policy`.
  Rows 1/2/3 of Next Steps, by contrast, remove *input* tokens/gaze cost; the edit
  tool removes *output* tokens, which are the expensive side on a small change.
- "CTL + C" or "ESC" to cancel current task. 
- TUI: use ratatui to improve the user experience. 
- Show thinking / reasoning text: rendered as a live block of at most `THINKING_LINES`
  (5) rows instead of a scrolling log — the first four rows carry the thread of the
  thought and the last one is what the model is thinking now, with the dropped middle
  marked by `…`. Each chunk *replaces* the block rather than appending to it, so a
  model that thinks for thousands of characters cannot bury the answer that follows
  (`Diff::Replace`, `view::thinking_lines`). The 5-line cap is a display decision; the
  text held in memory is bounded separately by `REASONING_WINDOW`. Toggled with
  `Ctrl-R` (TUI) or `/thinking on|off`. The full text is still stored and exported.
- Status line: `Tokens: … | Cxt: … / … (%) | Time Used: …`, using provider-reported
  usage when available and a local estimate otherwise. Provider numbers win for task
  accounting; the context figure is estimated up front so it is on screen before the
  first turn.
- Bounded tool loop: `agent.max_tool_rounds` caps assistant/tool round trips per user
  turn. The hard stop against a model that never converges was already enforced, but
  silently — exhausting the budget now emits a notice naming the setting, so a turn
  that stops mid-task cannot be mistaken for a crash. Validated (>= 1) in
  `Config::validate`, documented in `config.example.toml`.

## In Progress


## Known Issues
- Pre-existing test failures, present before this work and untouched by it (verified
  by stashing): `tokens::tests::wide_characters_count_as_one_token` and
  `the_estimator_is_used_when_nothing_is_reported` (estimator vs. expectations),
  `tokens::tests::explicit_size_suffixes_win_over_the_table` (8k parses to 8,000 but
  the test wants 8,192), `cli::sessions_can_be_deleted_and_purged` (the two-session
  fixture now resumes instead of creating a second session),
  and the `jai-db` migration tests. These keep CI red independently of this branch.


## Next Steps
- `cargo fmt --check` passes; clippy is at parity with before this work.

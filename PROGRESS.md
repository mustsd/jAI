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


## In Progress


## Known Issues



## Next Steps
- TUI: use ratatui to improve the user experience. 
- show thinking or reasoning text. 
- Show token usage, current context usage, and elapsed time: "Tokens: ~12,400 | Cxt: 345,000 / 1,000,000 (34.5%) | Time Used: 2m 124.5s". Task usage including input and output tokens context usage estimate the active context, including instructions and tools, rather than saved logs. Use provider usage whenever available, otherwise an estimate (such as tokenizer).  
- Try to restore the last session if possible when the current path is the same as the workspace path. Report whether a session resumed.
- Double check and modify if needed: Save prompts, replies, tool calls/results, and active context. Do not save API keys. 
- "CTL + C" or "ESC" to cancel current task. 
- Check tool "filesystem_write" if a edit tool is needed. 

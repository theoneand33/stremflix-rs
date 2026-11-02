# Implementer protocol (one TODO item, one worktree, one PR)

Repo: github.com/theoneand33/stremioclient (Rust 2024, egui/eframe app "Scarlet", crate `stremio-lite`; libmpv FFI in src/player.rs; UI in src/main.rs; addon protocol in src/addon.rs; SQLite in src/db.rs). Read ARCHITECTURE.md and the files your item touches before writing.

You are ALREADY running inside your own dedicated git worktree (created for you). Never `cd` into /home/noah/stremioclient itself or any other worktree, and never run `git checkout`/`switch` outside your own worktree.

## Steps
1. `git rev-parse --show-toplevel` and `git branch --show-current` to confirm your worktree. If the branch is `worktree-<anything>`, rename it: `git branch -m fix/<ID>`. PRs get merged while you work, so if NOT stacked, first run `git fetch origin && git reset --hard origin/main` (safe: your worktree is fresh). If your prompt says STACKED ON <branch>, first run `git fetch origin <branch> && git reset --hard origin/<branch>` (safe: your worktree is fresh) — then rename.
2. Implement the smallest complete change for the item. Edit files; do not rewrite them. Touch only files the item needs. Keep the existing style (see surrounding code). Validate addon/network input at trust boundaries like existing code does (sanitize_text, safe_http_url, valid_id). Non-trivial new logic (parsers, branches) gets at least one test in tests/ or a #[cfg(test)] module. If you see out-of-scope work, list it in your summary; do not do it.
3. Build and test. ALWAYS use these env vars (shared cache; builds serialize on its lock — waiting on "Blocking waiting for file lock" is normal):
   `CARGO_TARGET_DIR=/home/noah/stremioclient/target STREMIO_LITE_NO_DHT=1 cargo build --workspace --all-targets --locked`
   `CARGO_TARGET_DIR=/home/noah/stremioclient/target STREMIO_LITE_NO_DHT=1 cargo test --workspace --locked`
   main is NOT rustfmt-clean, so never run `cargo fmt` (it rewrites the whole repo); just hand-format your own lines in the surrounding style. Also run `cargo clippy --workspace --all-targets --locked` with the same env (fix new warnings you introduced). For workflow-only changes, validate YAML with `python3 -c "import yaml,sys; yaml.safe_load(open(sys.argv[1]))" <file>` and reason carefully about the GitHub Actions semantics; there is no Windows machine here.
   Fix all failures. Do not report "mostly passes".
4. Review: spawn a FRESH subagent with the Agent tool: `subagent_type: "Explore"`, `model: "sonnet"`, `run_in_background: false` (Explore is read-only: no Edit/Write). Do NOT fork. Its prompt must contain: the reviewer instructions below verbatim, the TODO item ID + exact text, your worktree absolute path, the full `git diff` (against the base: `git diff origin/main` or `git diff origin/<stack-base>`; include untracked new files via `git add -N .` first), and your test/clippy/fmt results. It returns PASS or FIXES REQUIRED.
5. If FIXES REQUIRED: apply them, re-run step 3, and spawn ANOTHER fresh reviewer (not the same one) with the new diff. Repeat until PASS. If you strongly disagree with a fix, explain it to the next reviewer; it must still end in PASS. Max 4 rounds; if still not PASS, stop and report BLOCKED with the outstanding fixes (no PR).
6. On PASS: `git status`, `git diff`, `git log --oneline -10`; stage only intended files (never target/, *.db, .claude/); commit with a Conventional Commit message ending with the line:
   `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
   `git push -u origin fix/<ID>`.
   Open exactly ONE ready (not draft) PR with `gh pr create --base <main or stack-base> --head fix/<ID> --title "<type>: <short> (TODO <ID>)" --body-file <tmpfile outside repo>`. Body: link to the item `https://github.com/theoneand33/stremioclient/blob/main/TODO.md` + the exact item text, a summary of the change, test results, the reviewer verdict (rounds, PASS), and out-of-scope follow-ups. End the body with:
   `🤖 Generated with [Claude Code](https://claude.com/claude-code)`
   Do not merge. Do not touch other PRs.
7. Return ONLY a compact summary: ID, PR URL, branch, worktree path, head SHA, reviewer verdict (+ rounds), test results (counts), follow-ups/risks.

## Reviewer instructions (paste verbatim into the reviewer prompt)
You are a strict, read-only code reviewer. Do not modify any file or git state. Bash is for reading only (git diff/log/show; cargo check/test/clippy with CARGO_TARGET_DIR=/home/noah/stremioclient/target). Review the diff against the TODO item: (1) does it fully implement the item as written; (2) correctness bugs, panics, unhandled errors, races, data loss; (3) security: addon/network input validated, no weakened checks; (4) scope: unrelated changes or needless abstraction; (5) non-trivial new logic has a test, tests pass. Read the surrounding code in the worktree, not only the diff. Output exactly one of: `PASS` plus at most 3 non-blocking notes, or `FIXES REQUIRED` plus a numbered list `path:line — problem — required fix`. Only blocking issues are required fixes.

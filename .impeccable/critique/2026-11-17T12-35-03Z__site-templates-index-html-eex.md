---
target: armless site (landing + docs)
total_score: 27
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 3
target_identity: "file:/Users/filipecabaco/Documents/workspace/armless/site/templates/index.html.eex"
target_fingerprint: "sha256:6331447bf40be04a8737fffc6c6fac01e37ad22f8c1e118ba718eb5be0199eac"
target_path: /Users/filipecabaco/Documents/workspace/armless/site/templates/index.html.eex
timestamp: 2026-11-17T12-35-03Z
slug: site-templates-index-html-eex
---
# Critique: armless site (landing + docs), minimalist dark direction

Score 27/40 (Good). No heuristic n/a.

| # | Heuristic | Score |
|---|---|---|
| 1 | Visibility of system status | 2 |
| 2 | Match system / real world | 3 |
| 3 | User control and freedom | 2 |
| 4 | Consistency and standards | 3 |
| 5 | Error prevention | 3 |
| 6 | Recognition rather than recall | 3 |
| 7 | Flexibility and efficiency | 2 |
| 8 | Aesthetic and minimalist design | 4 |
| 9 | Error recovery | 2 |
| 10 | Help and documentation | 3 |

Specificity: authored for armless; the memory pane on vm.memory_report() and the rm /tmp/ballast drop are the peak. Detector: 1072 findings, nearly all false positives (oklch tokens unresolved; padded children). Em-dash counts real in README-derived docs.

## Priority issues
- [P1] Auto-typed loop sometimes loses its start (timing); status claims it runs; console_resize before wasm ready throws. Fix: wait for the prompt, set status on first "pass" line, guard resize. harden
- [P1] Terminal is a keyboard trap (Tab/Shift+Tab/Esc go to the guest); no output for screen readers; label on a role-less div; ^C read as "caret C"; no focus ring. harden
- [P1] Status goes stale after ctrl+c; "loading" forever without JavaScript. harden
- [P2] pre renders in UA monospace not Space Mono; inline code breaks flags mid-token (overflow-wrap: anywhere). typeset
- [P2] Mobile docs: nav before content (h1 at 551px), no TOC under 76rem; .keys .btn 32px on touch. adapt

## Minor
Hero pushes chart below the fold at 1440x900; silent loading; hidden-tab gaps drawn as data; reduced motion ignored by cursor blink and typing; pager wider than prose; landing h2 same size as body.

## Questions
Headline vs user-mode demo; mark the take-over moment on the chart; per-option links in the API docs.

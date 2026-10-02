# SSH login scripts

In a connection's **Terminal** section, **Run after connect** accepts multiple
lines of remote shell input, including conditions, loops and here-documents.
The commands execute in the interactive terminal, so changes to the directory
and environment remain available there.

**Login steps** run afterwards, in their displayed order. Each step waits for a
literal substring or regular expression in terminal output, then sends its
commands and Enter. Leave the wait empty to send immediately. Use actual line
breaks in commands; shell backslashes are preserved.

For example, leave the initial command empty and configure:

| Wait | Send | Regex |
| --- | --- | --- |
| `[$#] $` | `cd /srv/app` | Yes |
| `[$#] $` | `if [ -f .envrc ]; then`<br>`  . ./.envrc`<br>`fi` | Yes |

Patterns use Rust's `regex` syntax. Output matching ignores terminal control
sequences and handles prompts and UTF-8 characters split across packets. Each
match is consumed once. An optional step is skipped only when a later reachable
prompt matches, never merely because the current packet lacks its prompt.
Waiting has no timeout; type in the terminal to cancel the remaining steps and
continue manually.

The terminal parser owns each run. It starts after PTY allocation, stops on input,
EOF or terminal closure, and does not own the shared SSH node. Splitting a pane
does not automatically replay the script. Explicitly opening a terminal from the
connection uses the current saved script.

Login responses are redacted in diagnostics and cleared when their owner is
dropped. Saving steps uses the existing encrypted connection store. Plaintext
connection snapshots, including metadata cloud sync and CLI backups, omit them
and preserve a destination's local script. To transfer steps, use an encrypted
`.oxide` export with credentials included.

Limits are 64 steps, 8192 UTF-8 bytes per wait/send field, and a 16 KiB output
matching window. Invalid patterns and oversized fields are rejected before the
connection form is submitted. A runtime failure stops automation and shows a
terminal notice, allowing the user to continue manually.

## Terminal throughput check

Measured on Darwin arm64 on 2026-09-29, using the existing SSH worker benchmark
in the unoptimized test profile, four runs of approximately 16 MiB per workload.
Run with `cargo test -p oxideterm-terminal ssh_background_performance -- --ignored --nocapture`.
The original `integration` executable at `a789fb21b` and the updated executable
ran sequentially without concurrent compilation. Median parser time:

| Workload | Baseline (ms) | Updated (ms) | Change |
| --- | ---: | ---: | ---: |
| Plain text | 914.19 | 911.17 | -0.33% |
| ANSI | 1598.71 | 1613.99 | +0.96% |
| Unicode | 1752.51 | 1635.90 | -6.65% |
| Long CSI | 1344.45 | 1342.86 | -0.12% |

These runs check the ordinary terminal path with no active login script. They
show no substantial throughput regression; the Unicode baseline varied between
runs, so its lower updated median is not evidence of an optimization.

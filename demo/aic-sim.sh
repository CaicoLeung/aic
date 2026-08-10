#!/usr/bin/env bash
# Simulates aic's hunk-split output for the demo GIF.
# Colors match aic's WCAG-tested palette (src/types.rs NAMED_PALETTE).
set -euo pipefail

B=$'\033[1m'
R=$'\033[0m'
GRAY=$'\033[38;2;107;114;128m'
FEAT=$'\033[38;2;21;128;61m'    # green-700   #15803d
FIX=$'\033[38;2;234;88;12m'     # orange-600  #ea580c
STYLE=$'\033[38;2;124;58;237m'  # violet-600  #7c3aed
AMBER=$'\033[38;2;217;119;6m'   # amber-600   #d97706
DIM=$'\033[2m'
GREEN=$'\033[38;2;21;128;61m'

frames=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")

# --- Phase 1: spinner + reasoning ---
for i in $(seq 0 11); do
  secs=$((i / 5 + 1))
  printf "\r  %s Analyzing changes… %ds" "${frames[$((i % 10))]}" "$secs"
  sleep 0.1
done
printf "\r  %s Analyzing changes… 3s\n" "${frames[3]}"
sleep 0.15

printf "  ${DIM}│ 3 hunks in src/auth.rs — splitting by concern${R}\n"
sleep 0.22
printf "  ${DIM}│ hunk 1 (L42-48): token-expiry check is inverted${R}\n"
sleep 0.22
printf "  ${DIM}│ hunk 2 (L120-180): new OAuth2 login provider${R}\n"
sleep 0.22
printf "  ${DIM}│ hunk 3 (L1-10): import block needs reordering${R}\n"
sleep 0.2

# --- Phase 2: commit results ---
printf "  ${GRAY}[1/3]${R} ${GREEN}${B}✓${R} ${AMBER}${B}a1b2c3d${R} ${FIX}${B}fix${R}${GRAY}(auth)${R}${B}: correct token expiry check${R}\n"
sleep 0.35
printf "  ${GRAY}[2/3]${R} ${GREEN}${B}✓${R} ${AMBER}${B}c4d5e6f${R} ${FEAT}${B}feat${R}${GRAY}(auth)${R}${B}: add OAuth2 login provider${R}\n"
sleep 0.35
printf "  ${GRAY}[3/3]${R} ${GREEN}${B}✓${R} ${AMBER}${B}7a8b9c0${R} ${STYLE}${B}style${R}${GRAY}(auth)${R}${B}: tidy imports${R}\n"
sleep 0.25
printf "\n"
printf "  ${DIM}3 atomic commits from 1 file — each concern isolated${R}\n"

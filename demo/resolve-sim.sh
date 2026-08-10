#!/usr/bin/env bash
# Simulates `aic resolve` output for the demo GIF.
# Output format matches src/main.rs:run_resolve_workflow_impl + src/display.rs.
set -euo pipefail

R=$'\033[0m'
DIM=$'\033[2m'
YELLOW=$'\033[33m'
YELLOW_B=$'\033[1;33m'
GREEN=$'\033[32m'
GREEN_B=$'\033[1;32m'
RED=$'\033[31m'
CYAN_B=$'\033[1;36m'
GRAY=$'\033[38;2;107;114;128m'

frames=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")

# --- Phase 1: conflict detection ---
printf "  ${YELLOW_B}⚠${R} conflicts detected — repo is mid-${YELLOW}merge${R} (2 files)\n"
sleep 0.6
printf "  ${GRAY}src/config.rs${R}\n"
sleep 0.15
printf "  ${GRAY}src/parser.rs${R}\n"
printf "\n"
sleep 0.8

# --- Phase 2: spinner while resolving (feels like real LLM thinking) ---
for i in $(seq 0 19); do
  printf "\r  %s ${DIM}Resolving src/config.rs${R}" "${frames[$((i % 10))]}"
  sleep 0.1
done
printf "\r  ${DIM}Resolving src/config.rs${R}      \n"
sleep 0.3

for i in $(seq 0 16); do
  printf "\r  %s ${DIM}Resolving src/parser.rs${R}" "${frames[$((i % 10))]}"
  sleep 0.1
done
printf "\r  ${DIM}Resolving src/parser.rs${R}      \n"
printf "\n"
sleep 0.6

# --- Phase 3: review diff ---
printf "  ${DIM}proposed resolutions:${R}\n"
sleep 0.25
printf "  ${CYAN_B}src/config.rs${R}\n"
printf "  ${RED}-timeout_secs = 30${R}\n"
printf "  ${GREEN}+timeout_secs = 60${R}\n"
sleep 0.2
printf "  ${CYAN_B}src/parser.rs${R}\n"
printf "  ${RED}-fn parse(input: &str) -> Vec<String> {${R}\n"
printf "  ${GREEN}+fn parse(input: &str) -> Result<Config> {${R}\n"
printf "\n"
sleep 1.2

# --- Phase 4: per-file approval ---
printf "${YELLOW}apply src/config.rs?${R} ${DIM}[Y/n]${R} "
sleep 0.5
printf "${GREEN_B}y${R}\n"
sleep 0.3
printf "  ${GREEN_B}✓${R} resolved + staged: src/config.rs\n"
sleep 0.6

printf "${YELLOW}apply src/parser.rs?${R} ${DIM}[Y/n]${R} "
sleep 0.5
printf "${GREEN_B}y${R}\n"
sleep 0.3
printf "  ${GREEN_B}✓${R} resolved + staged: src/parser.rs\n"
sleep 0.8

# --- Phase 5: finalize ---
printf "\n"
sleep 0.3
printf "  ${GREEN_B}✓ merge finalized${R}\n"

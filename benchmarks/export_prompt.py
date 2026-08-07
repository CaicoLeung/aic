#!/usr/bin/env python3
"""Export SYSTEM_PROMPT_GIT_MESSAGE from src/prompt.rs to prompt-baseline.txt
so the benchmark baseline always tracks the shipped prompt."""
import re, sys
from pathlib import Path

src = Path("src/prompt.rs").read_text()
m = re.search(r'const SYSTEM_PROMPT_GIT_MESSAGE: &str = r#"\n(.*?)\n"#;', src, re.S)
if not m:
    sys.exit("could not find SYSTEM_PROMPT_GIT_MESSAGE in src/prompt.rs")
Path("benchmarks/prompt-baseline.txt").write_text(m.group(1).strip() + "\n")
print("exported", len(m.group(1)), "chars to benchmarks/prompt-baseline.txt")

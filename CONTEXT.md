# aic

aic is an AI-powered git commit tool: it reads a diff, drafts a conventional-commit message, and commits it. When nothing is staged it groups unstaged work into logical commits and commits each group.

## Language

**Run**:
One execution of the default commit workflow — either a single commit over staged files, or a batch plan over unstaged files.
_Avoid_: execution, invocation, session

**Batch**:
A group of files the LLM's split plan assigns to one commit. A Run contains one or more Batches; each Batch yields one Drafted Message and one commit.
_Avoid_: group, chunk, package

**Drafted Message**:
The conventional-commit message (with optional body) the LLM produces for a Batch's diff.
_Avoid_: suggestion, proposal, generated text

**Provider**:
An LLM backend the user configures (OpenAI, Anthropic, Gemini, DeepSeek, Groq, Ollama). Resolved per Run from env, then config, then default.
_Avoid_: backend, engine

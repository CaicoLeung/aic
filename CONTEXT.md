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
A named LLM backend the user can route a Run through (OpenAI, Anthropic, Gemini, DeepSeek, Groq, Ollama, xAI, Mistral, OpenRouter, Perplexity, Together, plus the generic OpenAI-compatible provider). Resolved per Run from env, then config, then default.
_Avoid_: backend, engine

**Base URL**:
The endpoint a Provider sends requests to. Optional and overridable (env/config); required for the OpenAI-compatible provider. Defaults to each Provider's canonical API host.
_Avoid_: endpoint, server, URL

**OpenAI-compatible provider**:
A generic Provider that routes through the OpenAI client against a user-supplied Base URL, for servers that speak the OpenAI chat-completions API (LM Studio, vLLM, gateways). Has no Default Model — the user must supply one.
_Avoid_: custom provider, generic provider, passthrough

**Default Model**:
The model used for a Provider when the user has not set one (env/config). Chosen for speed and cost, since aic's workload (commit messages) is lightweight.
_Avoid_: fallback model, base model

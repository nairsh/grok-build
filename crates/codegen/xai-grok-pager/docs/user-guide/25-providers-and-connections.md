# Providers and Connections

Grok is multi-provider. Beyond the built-in SpaceXAI models you can point models
at OpenAI, Anthropic, OpenRouter, any OpenAI/Anthropic-compatible endpoint, and —
via subscription login — Claude Pro/Max, ChatGPT (Codex), and GitHub Copilot.

The model layer separates four independent ideas so **a provider is not an
account**:

| Concept | What it is |
|---------|------------|
| **Adapter** | The wire protocol (`chat_completions`, `responses`, `messages`). |
| **Connection** | A reusable `{adapter, endpoint, credential}` bundle, named in `[connection.*]`. |
| **Credential** | How to authenticate — an API key or a stored subscription — referenced by id. |
| **Model** | References a connection (`connection = "<id>"`); its own fields still win. |

Because a credential is referenced independently, you can have **multiple
independent connections for the same provider** — e.g. an xAI personal
subscription, an xAI work key, and an xAI project key — without special-casing
anything.

---

## Quick start: use an API-key provider

Grok ships **built-in connections** for common providers, keyed off the
conventional environment variable. Just export the key and reference the
connection from a model:

```bash
export OPENAI_API_KEY=sk-...
```

```toml
# ~/.grok/config.toml
[model.gpt-5.1]
connection = "openai"
model = "gpt-5.1"
context_window = 400000
```

```
/model gpt-5.1
```

Built-in connections:

| Connection id | Adapter | Endpoint | Env var |
|---------------|---------|----------|---------|
| `openai` | `responses` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `anthropic` | `messages` | `https://api.anthropic.com/v1` | `ANTHROPIC_API_KEY` |
| `openrouter` | `chat_completions` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |

A user-defined `[connection.<id>]` with the same id fully overrides the built-in
(e.g. to route through a proxy).

---

## Defining your own connection

Declare a `[connection.<id>]` block, then reference it from any number of models:

```toml
[connection.anthropic-work]
adapter = "messages"
base_url = "https://api.anthropic.com/v1"
auth_scheme = "x_api_key"
credential = { env = "ANTHROPIC_WORK_KEY" }
extra_headers = { "anthropic-version" = "2023-06-01" }

[model.claude-opus]
connection = "anthropic-work"
model = "claude-opus-4-8"
context_window = 200000
```

### Credential forms

The `credential` field accepts:

```toml
credential = "xai"                       # built-in xAI resolution (default)
credential = { api_key = "$OPENAI_API_KEY" }
credential = { env = "ANTHROPIC_API_KEY" }
credential = { env = ["LC_TOKEN", "TOKEN"] }   # first set value wins
credential = { oauth = "anthropic" }     # a stored subscription (see below)
credential = { named = "my-saved-key" }  # a stored API key
credential = "none"                      # no credential (e.g. keyless local server)
```

API-key values support environment interpolation and command execution, matching
the wider config convention:

- `"$ENV_VAR"` / `"${ENV_VAR}"` — interpolate an environment variable.
- `"!command"` — run a command; its stdout is the key (e.g.
  `"!op read 'op://vault/item/credential'"`).
- `"$$"` / `"$!"` — emit a literal `$` / `!`.
- anything else — a literal.

### Precedence

A connection provides the **base**; a model's own `[model.*]` fields always win.
So a model can share a connection but override, say, `base_url` or `api_backend`
for itself.

---

## Subscription login (Claude Pro/Max, ChatGPT, Copilot)

Grok can authenticate against provider **subscriptions** using OAuth, storing the
tokens in `~/.grok/credentials.json` (0600) and refreshing them automatically.

| Subscription | Credential id | Flow |
|--------------|---------------|------|
| Claude Pro/Max | `anthropic` | Browser (PKCE loopback) |
| ChatGPT (Codex) | `openai-codex` | Browser (PKCE loopback) |
| GitHub Copilot | `github-copilot` | Device code |

Once logged in, reference the stored subscription from a connection:

```toml
[connection.claude-sub]
adapter = "messages"
base_url = "https://api.anthropic.com/v1"
credential = { oauth = "anthropic" }

[model.claude-sub]
connection = "claude-sub"
model = "claude-sonnet-4-8"
context_window = 200000
```

> **Note:** subscription OAuth flows are ported from the open-source Pi Agent
> Harness. Third-party subscription usage is billed by the provider per their
> terms. These flows require your own live accounts to complete; verify with a
> real login before relying on them.

---

## How resolution works

At model load, `resolve_model_list` seeds a connection-referencing model with the
connection's endpoint/adapter/credential as the base, then applies the model's
own fields on top. API-key and env credentials become the model's inline key;
`oauth`/`named` credentials resolve to a live bearer from the credential store at
request time. The xAI default path is unchanged — existing configs keep working
with no migration.

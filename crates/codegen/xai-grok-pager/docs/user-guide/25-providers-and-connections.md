# Providers and Connections

Atlas is multi-provider. Beyond the built-in xAI models, it includes 26
API-key provider presets plus arbitrary OpenAI/Anthropic-compatible endpoints.
Claude Pro/Max and ChatGPT (Codex) subscription login are also supported.

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

## Quick start

Run:

```bash
atlas login
```

Choose a subscription, API-key provider, or custom endpoint. You can jump
directly to a provider with `atlas login openrouter`. From a running session,
`/login` opens a native provider-management window and keeps the session
intact. It shows whether each provider has a saved key, subscription login, or
environment key (without displaying the secret). Press `Enter` to run that
provider's setup flow, `d` to remove a saved credential, and `r` to refresh
the status. `/logout` opens the same window in a removal-only view, containing
only saved credentials.

For API keys, Atlas hides the key while you type, suggests the provider's
current default model, stores the secret in `~/.grok/credentials.json`, and adds
a complete connection plus model entry to `~/.grok/config.toml`. Restart Atlas
after adding a provider from the running TUI so the agent reloads its model
catalog.

Atlas also ships built-in connections keyed off conventional environment
variables. This remains useful for scripted or ephemeral setups:

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

Built-in API-key connections:

| Connection ids | Env vars |
|---|---|
| `openai`, `anthropic`, `openrouter`, `litellm` | `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `LITELLM_API_KEY` |
| `google`, `deepseek`, `groq`, `cerebras`, `nvidia` | `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, `CEREBRAS_API_KEY`, `NVIDIA_API_KEY` |
| `zai`, `zai-coding-cn`, `mistral` | `ZAI_API_KEY`, `ZAI_CODING_CN_API_KEY`, `MISTRAL_API_KEY` |
| `minimax`, `minimax-cn` | `MINIMAX_API_KEY`, `MINIMAX_CN_API_KEY` |
| `moonshotai`, `moonshotai-cn`, `kimi-coding` | `MOONSHOT_API_KEY`, `MOONSHOT_API_KEY`, `KIMI_API_KEY` |
| `huggingface`, `fireworks`, `together` | `HF_TOKEN`, `FIREWORKS_API_KEY`, `TOGETHER_API_KEY` |
| `vercel-ai-gateway`, `ant-ling` | `AI_GATEWAY_API_KEY`, `ANT_LING_API_KEY` |
| `xiaomi`, `xiaomi-token-plan-cn`, `xiaomi-token-plan-ams`, `xiaomi-token-plan-sgp` | `XIAOMI_API_KEY`, `XIAOMI_TOKEN_PLAN_CN_API_KEY`, `XIAOMI_TOKEN_PLAN_AMS_API_KEY`, `XIAOMI_TOKEN_PLAN_SGP_API_KEY` |

If one of these environment variables is set, Atlas automatically adds that
provider's default model to `/model`; a hand-written model block is no longer
required for the first model. User-defined connections and models still take
precedence.

The preset registry is adapted from Pi's provider catalog. Atlas currently
ports providers that fit its Responses, Chat Completions, or Anthropic Messages
adapters. Amazon Bedrock, Google Vertex, Azure OpenAI Responses, Cloudflare's
account-scoped endpoints, and native provider transports remain separate
follow-up work rather than being advertised as compatible presets.

LiteLLM Proxy is included as an OpenAI-compatible preset (`http://localhost:4000/v1` by
default, and `/login litellm` lets you override it). When adding LiteLLM or another OpenAI-compatible custom endpoint,
Atlas attempts `GET /models` using the entered key and saves all returned model
ids for that connection. If a gateway disables model listing, setup falls back
to a manually entered model id.

OpenRouter intentionally does not fetch its full public catalog. Its setup form
accepts a comma-separated list of the exact model ids you want to enable, and
adds only those models to `/model`.

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

Atlas can authenticate against provider **subscriptions** using OAuth, storing
the tokens in `~/.grok/credentials.json` (0600) and refreshing expired tokens at
startup.

| Subscription | Credential id | Flow |
|--------------|---------------|------|
| Claude Pro/Max | `anthropic` | Browser (PKCE loopback) |
| ChatGPT (Codex) | `openai-codex` | Browser (PKCE loopback) |
| GitHub Copilot | `github-copilot` | Not yet implemented |

Successful subscription login is immediately usable. Atlas creates built-in
subscription connections automatically:

- ChatGPT models are fetched from the account-aware Codex model endpoint. Atlas
  declares a tested Codex catalog compatibility level, ignores hidden/internal
  models, and keeps the last successful non-empty catalog for offline startup.
  A bundled model remains available if discovery has never succeeded.
- Claude Pro/Max currently uses a bundled subscription model definition.

> **Note:** subscription OAuth flows are ported from the open-source Pi Agent
> Harness. Third-party subscription usage is billed by the provider per their
> terms. These flows require your own live accounts to complete; verify with a
> real login before relying on them.

---

## How resolution works

At model load, `resolve_model_list` seeds a connection-referencing model with the
connection's endpoint/adapter/credential as the base, then applies the model's
own fields on top. API-key, environment, OAuth, and named credentials become the
model's effective request credential. The xAI default path is unchanged, so
existing configs keep working with no migration.

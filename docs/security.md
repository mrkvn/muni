# Muni — Security & Privacy Model

This document describes how Muni handles secrets and user data, and the
threat model it is (and is not) designed for. It is written for users
deciding whether to trust the app and for future contributors auditing it.

## Threat model

Muni is a **local-first, single-user desktop app**. The trust boundary is
the user's macOS account: anything that account can already read is not
treated as a secret *from that account*. The app defends against:

- secrets (API keys) leaking off the machine — into logs, error messages,
  the app bundle, or network requests where they don't belong;
- the user's dictation content leaking off the machine or into shared logs;
- a compromised webview being able to reach dangerous native capabilities.

It does **not** attempt to defend the user's own dictation history from
someone who already has full access to the user's logged-in account or an
unlocked, unencrypted disk. Rely on macOS FileVault for at-rest protection
of the whole account.

## Secrets (API keys)

Muni is **bring-your-own-key (BYOK)**. The user supplies their own provider
keys (Deepgram, Groq, and optionally Gemini / Gladia).

- Keys are stored in the **OS keychain** (`com.muni.app`), never in the app
  bundle, never in a plaintext file, never in the SQLite database. See
  `src-tauri/src/secrets.rs`.
- A developer env-var override (`MUNI_*_KEY`) exists for local workflows and
  wins over the keychain on read. It is a dev convenience only.
- No IPC command returns a key value to the webview — only boolean
  "is this key set?" probes are exposed.
- **There is no app-owned shared secret bundled in the binary.** This is why
  Muni does not use binary obfuscation: there is nothing secret in the
  binary to protect. (The Rust core already compiles to native code and the
  frontend is minified; obfuscation would add cost without security value.)
- One value *is* compiled into release binaries: `MUNI_PRICES_TOKEN`, a
  read-only bearer token for the optional price-feed service
  (`src-tauri/src/prices_client.rs`). Treat it as public — keep it
  read-only, narrowly scoped, and rate-limited server-side. The app
  functions without it (it falls back to bootstrap price rows).

## Network

All provider calls are made from the Rust backend (never the webview):

- Transport is always TLS — `https://` / `wss://`. There are no plaintext
  endpoints and no certificate-verification bypasses.
- API keys travel in `Authorization` / provider-specific headers, never in
  URL query strings (which leak via proxies and logs).
- Error response bodies are length-capped before being logged.

Hosts contacted: `api.deepgram.com`, `api.groq.com`, `api.gladia.io`,
`generativelanguage.googleapis.com`, and the optional price-feed host.

## Webview hardening

- A restrictive **Content-Security-Policy** is set in `tauri.conf.json`
  (`app.security.csp`). The webview loads only bundled local assets and
  makes no direct external network calls; the policy reflects that
  (`connect-src` is limited to `'self'` + Tauri IPC). A separate, looser
  `devCsp` allows the Vite HMR dev server.
- IPC capabilities are scoped per window (`src-tauri/capabilities/`). The
  HUD overlay gets a minimal surface; the main/onboarding windows get the
  app's command set.
- The `mcp-bridge` plugin (a dev-time DOM/CSS inspection bridge) is
  registered only under `#[cfg(debug_assertions)]`, so its commands are not
  wired into release builds even though the crate links in.

## Data at rest

Stored under `~/Library/Application Support/com.muni.app/`:

- `history.sqlite3` — dictation history (`raw_text`, `cleaned_text`,
  target-app bundle id, timestamps), per-call cost/usage rows, and cached
  pricing. **Stored in plaintext** — acceptable under the threat model
  above; for full-disk protection use FileVault.
- `settings.json` (Tauri store) — preferences, plus user-authored cleanup
  context (My Words, Vocabulary, About Me, User Prompt). Plaintext.

All SQL is parameterized (no injection). Settings keys are validated against
an allowlist, so the webview cannot write arbitrary keys or coerce file
paths.

### Deleting your data

- **History tab → Wipe history** deletes every record and then runs
  `VACUUM`, so the freed transcript bytes are rewritten out of the database
  file rather than lingering in its free list.
- Retention purging removes records older than the configured window on
  launch.
- To remove everything, delete the
  `~/Library/Application Support/com.muni.app/` directory and remove the
  keychain entries under the `com.muni.app` service.

## Developer-only switches (must stay off in distributed builds)

These are gated behind env vars and are **not** set in shipped builds. Do
not enable them in a build handed to other users:

- `MUNI_TRACE_GLADIA=1` — logs full Gladia WebSocket frames, which include
  transcript text.
- `MUNI_DEBUG_AUDIO=1` / `MUNI_DUMP_AUDIO_DIR=…` — writes raw captured audio
  (`.wav`) to disk.

## Logging

The shipped log floor is `Info`. Transcript text is only logged on targets
raised to `Debug`/`Trace` (`muni`, `lid`, opt-in `gladia`) — the default
provider paths log lengths/metadata, not content. Logs live in the Tauri
app log directory and rotate (1 MB, keep-one).

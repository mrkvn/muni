# Muni

Dictation app for macOS. Hold a hotkey, speak, release — cleaned-up text is pasted into whichever app has focus.

- **Cleanup pass** — filler words removed, "scratch that" corrections applied, grammar and punctuation fixed, your phrasing preserved.
- **Language auto-detect** — English, Tagalog, and Taglish presses are routed automatically, with the detection running locally on-device by default.

![Muni demo](assets/demo.gif)

## Download

- **Latest DMG:** [Releases](https://github.com/mrkvn/muni/releases/latest)
- **Website:** [usemuni.app](https://usemuni.app)

The app updates itself from the same release feed.

## Requirements

- macOS 14 (Sonoma) or later, Apple Silicon
- A **Deepgram** API key (bring your own)
- A **Groq** API key (bring your own)

Muni is BYOK: your keys go straight from your machine to the providers, and they are stored in the macOS Keychain. The onboarding wizard asks for them on first launch.

## How it works

1. **Capture** — microphone audio via `cpal` while the hotkey is held (default: hold **Control + Option**).
2. **Detect language** — a bundled whisper.cpp tiny model classifies the audio on-device (no network call) and routes the press.
3. **Transcribe** — English presses stream to Deepgram Nova-3 over WebSocket; Tagalog/Taglish presses go to Groq's `whisper-large-v3-turbo`.
4. **Clean** — the transcript goes to Groq's `gpt-oss` models (short presses `openai/gpt-oss-20b`, long presses `openai/gpt-oss-120b`).
5. **Inject** — the result is written to the clipboard and `Cmd+V` is synthesized. Your previous clipboard contents are restored afterwards.

## Build from source

Prerequisites: Rust stable (`rustup default stable`), pnpm 9 (`corepack enable && corepack prepare pnpm@9 --activate`), Xcode Command Line Tools.

```bash
make install    # pnpm install + cargo fetch
make dev        # run the Tauri dev shell
make build      # bundle a production .app (pnpm --filter desktop tauri build)
```

```bash
make test       # vitest (frontend) + cargo test (Rust unit + integration)
make lint       # tsc --noEmit + cargo clippy --all-targets -- -D warnings
make fmt-rust   # cargo fmt
```

Please run `make lint && make test` before opening a pull request. CI only covers the
frontend — the Rust side needs macOS, so it is checked locally rather than on a runner.

**Source builds contain zero telemetry.** The Sentry and PostHog keys are injected at build time and only exist in the official signed binaries; a build from this repo has no keys compiled in and sends nothing.

### Environment variables (dev overrides)

API keys normally live in the Keychain. For local development you can override any of them from the environment — when set, these win over the Keychain:

```bash
export MUNI_DEEPGRAM_KEY="dg-..."
export MUNI_GROQ_KEY="gsk_..."
export MUNI_GEMINI_KEY="..."      # only needed for the Gemini text-LID path
```

### Swapping the language-detection provider

Language ID sits behind a trait, so the provider is chosen at boot via `MUNI_LID_PROVIDER`:

```bash
# Default (also used when unset/empty): local whisper.cpp tiny-q5_1 over the
# raw audio — no network call, p95 ~67 ms classify on an M2 Pro.
export MUNI_LID_PROVIDER="audio_whisper_tiny"

# Text-LID alternatives: transcribe with Groq Whisper, then classify the
# transcript with a chat model.
export MUNI_LID_PROVIDER="groq"     # default model: openai/gpt-oss-120b
export MUNI_LID_PROVIDER="gemini"   # default model: gemini-2.5-flash-lite

# Override the model for the text-LID providers (ignored by the audio path,
# whose model is the bundled resource ggml-tiny-q5_1.bin).
export MUNI_LID_MODEL="gemini-2.5-flash"

# Log every Groq LID raw response body alongside the parsed label.
export MUNI_LID_VERBOSE="1"
```

All of these are read once at launch; switching providers requires a relaunch. The active provider is logged on boot (`[lid] boot: audio-LID provider=…` or `[lid] boot: text-LID provider=…`) and on every classify call.

### Cleanup warm-up

At boot — and whenever the cleanup prompt, About Me, vocabulary list, or Groq key changes — Muni fires a tiny fire-and-forget `chat/completions` call against the live cleanup prefix, so the first real press isn't the slowest of the session. Disable it with:

```bash
export MUNI_CLEANUP_WARMUP="false"
```

## Privacy & telemetry

Muni is local-first. Audio is streamed to your own provider accounts and nothing else; API keys stay in the Keychain and are never returned to the webview or written to logs.

Official binaries include crash reporting (Sentry) and anonymized usage analytics (PostHog), both with per-category consent toggles you can turn off at any time. Builds from source have no telemetry keys and send nothing.

- [`docs/security.md`](docs/security.md) — threat model, secret handling, data at rest
- [`docs/telemetry.md`](docs/telemetry.md) — what is collected, how it's scrubbed, how to opt out

## Support

Muni is free. If it's useful to you, you can [support development](https://usemuni.app/support) — pay what you like.

Bug reports and feature requests go to [GitHub Issues](https://github.com/mrkvn/muni/issues).

## License

MIT — see [`LICENSE`](LICENSE).

# Telemetry & privacy

Muni can send **crash reports** and **anonymous operational metrics** so the app
can be kept working. This document is the complete, honest description of what
that means: what is collected, what is deliberately never collected, how it is
scrubbed, how you turn it off, and why a build you compile yourself sends
nothing at all.

Everything described here is in this repository — the enforcement points are
linked so you can read the code rather than trust the prose.

---

## The privacy line (non-negotiable)

**Telemetry carries operational metadata only — never transcript content, never
audio, never anything derived from what you said.**

- No audio ever leaves the app for telemetry purposes. (Audio does go to your
  own ASR provider during dictation — that is the product, not telemetry, and it
  uses *your* API keys.)
- No transcript text, no clipboard contents, no window titles, no file contents.
- No email address, no name, no license key, no machine name, no IP address, no
  location.

This is enforced structurally, not by convention:

- Every PostHog event is built by a **typed constructor** with an explicit
  property **allowlist**
  ([`telemetry/events.rs`](../apps/desktop/src-tauri/src/telemetry/events.rs)).
  There is no API for attaching a free-form property map, so no call site can
  smuggle content onto the wire. A unit test fails the build if a constructor
  emits a property outside the allowlist.
- Every Sentry event passes a **`before_send` scrub** before transmission
  ([`telemetry/mod.rs`](../apps/desktop/src-tauri/src/telemetry/mod.rs)).

---

## The two categories (separate toggles)

| Category | What it is | Setting key | Default |
|---|---|---|---|
| **Crash reporting** | Crashes, panics and handled errors → Sentry | `telemetry.crash_reporting` | on |
| **Usage analytics** | Anonymous operational-health events → PostHog | `telemetry.analytics` | on |

Both are presented, pre-checked, on a consent step during first-run onboarding
(informed opt-out) and are mirrored in **Settings → General**. They are
independent: you can keep crash reporting and turn analytics off, or the
reverse, or turn both off.

### Turning it off takes effect immediately

Opting **out** is honoured live — no relaunch. Flipping a toggle off sets a
runtime gate that `before_send` (crashes) and the event emitter (analytics)
check on every single event, so sending stops at once.

Opting back **in** takes effect on the next launch, by design: if a subsystem
was off at startup it was never initialised, and there is no client or queue to
revive mid-session. Native crash capture runs in a separate monitor process that
likewise cannot be reconfigured at runtime, so it too is launch-gated.

---

## What crash reporting sends

A crash report contains a stack trace, the exception/panic message, the app
version, the OS, the build flavor, and the anonymous install id (below). Muni
additionally:

- **Scrubs home paths.** `/Users/<name>/…` becomes `/Users/<redacted>/…`
  everywhere it can appear: the message, exception values, the exception
  stacktrace, thread stacktraces, every frame's `filename`/`abs_path`/
  `package`/`module`, breadcrumb messages and data, and the absolute dylib paths
  in debug images. The match is **structural** (`/Users/` + any one path
  segment), so an unusual username — a UUID, a hex string, an email local-part —
  is redacted just as reliably as a common one.
- **Drops the hostname.** On macOS `server_name` is `<username>.local`, which is
  itself identifying, so it is removed outright.
- **Never sends provider response bodies.** Errors are captured through their
  user-facing message, which is built to surface a status code rather than the
  upstream body.
- **Leaves Sentry's own PII scrubbers on** (`send_default_pii = false`) as a
  backstop — no IPs, no cookies.
- **Sends no performance traces** (`traces_sample_rate = 0`).

Non-fatal events are sampled and throttled per fingerprint per day, so a
repeating error does not turn into a flood.

---

## What usage analytics sends

Events are operational-health signals. Continuous values are **bucketed** (e.g.
a latency band, a duration band, a character-count band) rather than sent
exactly, so no single event can fingerprint a particular utterance by its shape.

Events currently defined:

`app_launched` · `onboarding_started` · `permission_granted` ·
`dictation_completed` · `dictation_failed` · `dictation_empty` ·
`fallback_fired` · `scratch_that_used` · `my_words_changed` ·
`updater_download_stalled`

Two further constructors exist but are not wired to any call site today
(`first_dictation`, `rapid_retry`); they are listed for completeness and are
bound by the same allowlist.

The complete set of properties any event may carry (the allowlist):

- `source` (always `muni-desktop`), `environment` (`prod` | `dev`)
- `latency_bucket`, `audio_duration_bucket`, `char_count_bucket`
- `served_by`, `delivery`, `language_path`, `cleanup_model`, `degraded`
- `target_app_bundle_id` — bucketed to a small fixed list of common apps, or
  `other` / `unknown`; never an arbitrary bundle id
- `error_kind`, `empty_reason`, `fallback_provider`, `stall_reason`
- `permission` (which macOS permission was granted)
- `my_words_count` — a **count** of Substitutions entries, never the entries
- `app_version`, `os`

Note what is a *number* and never a *string*: `char_count_bucket` is how long
the result was, `my_words_count` is how many entries exist. The content itself
is never read by the telemetry layer.

Ingestion-level privacy:

- `$geoip_disable: true` — PostHog is told not to derive country/city/coordinates.
- `$ip: 0.0.0.0` — a non-routable placeholder overrides the connection IP, so
  your real IP is never stored (a null `$ip` would not have worked; PostHog
  falls back to the connection IP).
- `$process_person_profile: false` on all high-frequency events. Exactly one
  low-frequency event (`app_launched`) creates a person profile, carrying only
  `app_version` and `os`, so retention can be computed.
- `$insert_id` for idempotency, so an at-least-once retry does not double-count.

Events are queued durably on disk in the Rust backend and flushed in batches.
The queue is metadata only, same allowlist.

---

## The anonymous install id

Telemetry attaches exactly **one** identifier: a random v4 UUID generated on
first launch and stored in a small file in the app's local data directory
([`telemetry/install_id.rs`](../apps/desktop/src-tauri/src/telemetry/install_id.rs)).

It is not derived from anything about you or your machine — it is `uuid::new_v4()`.
It exists so a single install's crashes deduplicate and so retention can be
computed without knowing who you are. Delete the file (or the app's data
directory) and the install becomes a brand-new anonymous id.

---

## Official binaries only — builds from source send nothing

The Sentry DSN and the PostHog project key are **not in this repository**. They
are injected at build time, only for official production releases, via
`MUNI_BAKED_SENTRY_DSN` / `MUNI_BAKED_POSTHOG_KEY` (see
[`build.rs`](../apps/desktop/src-tauri/build.rs)) and `VITE_SENTRY_DSN` for the
webview half. The build script forwards whatever is present in the environment;
the code reads them with `option_env!`.

Consequently:

- **If you clone this repo and build Muni yourself, no key is baked in, no
  telemetry client is ever initialised, and nothing is sent — regardless of how
  the consent toggles are set.** The toggles still show, but there is no
  destination.
- Development and staging builds bake no keys either, so they are silent too.
- Only the signed binaries published on the official Releases page contain the
  keys.
- If you *want* telemetry pointed at your own Sentry project while hacking on
  Muni, export your own DSN as `MUNI_SENTRY_DSN` before launching — a runtime
  override that points the app at your project, not ours.

You can verify the absence yourself:

```bash
git grep -nE 'ingest\.[a-z.]*sentry\.io|phc_[A-Za-z0-9]{20,}'
```

This returns nothing (aside from `phc_test` fixtures in unit tests).

---

## Where the data goes

- **Crashes** → Sentry, in a project used solely for Muni.
- **Analytics** → PostHog Cloud (US region).

Both are third-party processors; their own retention and security policies
apply. Nothing described above is sold, and nothing is shared with anyone else.

---

## Questions

If you think something in this document is inaccurate, or you find a code path
that could send more than what is described here, that is a bug — please open an
issue. Privacy claims that the code does not enforce are the kind of bug we most
want to hear about.

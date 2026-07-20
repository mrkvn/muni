//! Feature 013 — "My Words": per-user deterministic substitution layer
//! applied between ASR finalization and the Groq cleanup pass.
//!
//! Each rule is `(trigger, replacement)`. The matcher is case-insensitive,
//! word-boundary anchored, gap-tolerant on the trigger (the gap between
//! trigger words matches one or more whitespace chars OR hyphens — so
//! ASR-hyphenated compounds like "plan-feature" still fire the "plan
//! feature" rule), and runs **without cascading**: rule B's replacement
//! is not re-scanned by rule C. This is achieved by a collect-then-stitch design
//! — every rule's matches are gathered against the *original* text, then
//! overlaps are resolved by longest-trigger-wins, then the output is
//! stitched in a single pass.
//!
//! A trigger may list multiple equivalent phrases separated by `|`
//! (e.g. `slash cpm|slush cpm|slash c p m`) that all map to the same
//! replacement. Each alternate keeps the same whitespace-tolerant +
//! word-boundary treatment as a single trigger. This is the user's
//! escape hatch for ASR variance like homophone mishears
//! ("slash" ↔ "slush") or acronyms returned as spaced letters
//! ("cpm" ↔ "c p m"). A literal `|` in a trigger isn't supported —
//! dictation never produces one.
//!
//! Hot path is read-heavy (every press) and write-rare (only on Settings
//! save), so the snapshot lives behind a `parking_lot::RwLock` and the
//! compiled rules are wrapped in an `Arc` so the press path takes one
//! read lock, clones the Arc, drops the lock, and works on the snapshot.

use std::sync::Arc;

use parking_lot::RwLock;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

use crate::settings::{KEY_MY_WORDS_ENABLED, KEY_MY_WORDS_ENTRIES, SETTINGS_FILE};

/// A single user-defined substitution rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MyWordsEntry {
    pub trigger: String,
    pub replacement: String,
}

/// Wire-format snapshot — exactly what IPC consumers see and persist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyWordsSnapshot {
    pub enabled: bool,
    pub entries: Vec<MyWordsEntry>,
}

impl Default for MyWordsSnapshot {
    fn default() -> Self {
        Self {
            enabled: true,
            entries: Vec::new(),
        }
    }
}

/// Hard cap on the number of persisted entries, enforced at load time.
/// Mirrors [`crate::vocabulary::MAX_ENTRIES`] — sized so a pathological
/// store file (hand-edited, corrupted, or written by a mismatched
/// version) can't force hundreds of regexes to compile on every press.
pub const MAX_ENTRIES: usize = 200;

#[derive(Debug)]
struct CompiledRule {
    regex: Regex,
    replacement: String,
    /// Byte length of the *original* trigger string. Used to break overlap
    /// ties — the rule with the longer trigger wins when two rules match
    /// the same span.
    trigger_len: usize,
}

#[derive(Debug)]
struct Inner {
    snapshot: MyWordsSnapshot,
    compiled: Arc<Vec<CompiledRule>>,
}

/// Managed state — one per process. Cheap to `Arc`-clone; the heavy
/// content lives behind the internal `RwLock`.
#[derive(Debug)]
pub struct MyWords {
    state: RwLock<Inner>,
}

impl Default for MyWords {
    fn default() -> Self {
        Self::from_snapshot(MyWordsSnapshot::default())
    }
}

impl MyWords {
    /// Test-only / boot-fallback constructor. Does not touch the store.
    pub fn from_snapshot(snapshot: MyWordsSnapshot) -> Self {
        let compiled = Arc::new(compile_entries(&snapshot.entries));
        Self {
            state: RwLock::new(Inner { snapshot, compiled }),
        }
    }

    /// Load initial state from the Tauri store. Read failures are treated
    /// as "no rules, enabled by default" with a logged warning — the app
    /// must never fail to boot just because My Words is misconfigured.
    pub fn from_app<R: Runtime>(app: &AppHandle<R>) -> Arc<Self> {
        let snapshot = read_snapshot_from_store(app);
        Arc::new(Self::from_snapshot(snapshot))
    }

    /// IPC read — returns a clone of the current snapshot.
    pub fn snapshot(&self) -> MyWordsSnapshot {
        self.state.read().snapshot.clone()
    }

    /// IPC write — validate, recompile, swap, persist.
    pub fn save<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        next: MyWordsSnapshot,
    ) -> Result<(), String> {
        let sanitized = sanitize_snapshot(next);
        let compiled = Arc::new(compile_entries(&sanitized.entries));

        // Persist BEFORE swapping the in-memory snapshot so a failed
        // write doesn't leave the press path running rules the user
        // can't see in the UI on next launch.
        let store = app
            .store(SETTINGS_FILE)
            .map_err(|e| format!("open store: {e}"))?;
        store.set(KEY_MY_WORDS_ENABLED, json!(sanitized.enabled));
        store.set(
            KEY_MY_WORDS_ENTRIES,
            serde_json::to_value(&sanitized.entries)
                .map_err(|e| format!("serialize entries: {e}"))?,
        );
        store.save().map_err(|e| format!("save store: {e}"))?;

        let mut guard = self.state.write();
        guard.snapshot = sanitized;
        guard.compiled = compiled;
        Ok(())
    }

    /// Pure hot-path substitution. Returns the input verbatim when the
    /// feature is disabled or no rules are compiled.
    pub fn apply(&self, text: &str) -> String {
        let (enabled, compiled) = {
            let guard = self.state.read();
            (guard.snapshot.enabled, Arc::clone(&guard.compiled))
        };
        if !enabled || compiled.is_empty() || text.is_empty() {
            return text.to_owned();
        }
        apply_compiled(&compiled, text)
    }
}

/// Greedy collect-then-stitch over the original text. See module docs.
fn apply_compiled(rules: &[CompiledRule], text: &str) -> String {
    // Candidate matches gathered against the *original* text.
    struct Candidate<'a> {
        start: usize,
        end: usize,
        replacement: &'a str,
        trigger_len: usize,
    }
    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    for rule in rules {
        for m in rule.regex.find_iter(text) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                replacement: rule.replacement.as_str(),
                trigger_len: rule.trigger_len,
            });
        }
    }
    if candidates.is_empty() {
        return text.to_owned();
    }
    // Resolve overlaps: prefer earlier start, then the longer *matched*
    // span (what actually fired against this text), then the rule's
    // declared trigger_len purely to keep the ordering deterministic
    // when spans tie. Using `trigger_len` (the rule's longest declared
    // alternate) as the primary tie-break is wrong: a rule can declare
    // a long alternate that never matches this input, which would
    // otherwise let its short actual match (e.g. the "cpm" alternate)
    // beat another rule's genuinely longer match (e.g. "cpm pro").
    candidates.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then((b.end - b.start).cmp(&(a.end - a.start)))
            .then(b.trigger_len.cmp(&a.trigger_len))
    });

    let mut accepted: Vec<&Candidate<'_>> = Vec::with_capacity(candidates.len());
    let mut cursor = 0usize;
    for c in &candidates {
        if c.start < cursor {
            continue;
        }
        accepted.push(c);
        cursor = c.end;
    }

    // Stitch.
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for c in accepted {
        out.push_str(&text[last..c.start]);
        out.push_str(c.replacement);
        last = c.end;
    }
    out.push_str(&text[last..]);
    out
}

fn read_snapshot_from_store<R: Runtime>(app: &AppHandle<R>) -> MyWordsSnapshot {
    let store = match app.store(SETTINGS_FILE) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(target: "my_words", "store open failed at boot: {e} — defaulting to empty list");
            return MyWordsSnapshot::default();
        }
    };
    let enabled = store
        .get(KEY_MY_WORDS_ENABLED)
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let entries: Vec<MyWordsEntry> = store
        .get(KEY_MY_WORDS_ENTRIES)
        .as_ref()
        .and_then(|v| serde_json::from_value::<Vec<MyWordsEntry>>(v.clone()).ok())
        .unwrap_or_default();
    sanitize_loaded_snapshot(MyWordsSnapshot { enabled, entries })
}

/// Load-time defence: normalize via [`sanitize_snapshot`] (trim, collapse
/// whitespace, drop empty triggers), then enforce [`MAX_ENTRIES`] by
/// truncating anything beyond it. A store file can end up oversized via
/// hand-editing, corruption, or a future version with a looser cap — the
/// app must never fail to boot over it, so this truncates rather than
/// errors. Extracted as a pure function (no `AppHandle`) so the cap is
/// unit-testable directly.
fn sanitize_loaded_snapshot(snap: MyWordsSnapshot) -> MyWordsSnapshot {
    let mut sanitized = sanitize_snapshot(snap);
    if sanitized.entries.len() > MAX_ENTRIES {
        log::warn!(
            target: "my_words",
            "loaded snapshot has {} entries, exceeding cap {MAX_ENTRIES} — truncating",
            sanitized.entries.len()
        );
        sanitized.entries.truncate(MAX_ENTRIES);
    }
    sanitized
}

/// Route a raw `my_words.entries` JSON value (an array of
/// `{trigger, replacement}` objects) through the same load-time sanitizer +
/// [`MAX_ENTRIES`] cap the store loader applies, returning the normalized
/// array. Lets the generic `settings_set` IPC path enforce the same defence
/// as the dedicated `my_words_save` command, so a compromised webview can't
/// smuggle unsanitized (or unbounded) rules straight into the store. Errors
/// when the value isn't a well-formed entries array.
pub fn sanitize_entries_value(value: &Value) -> Result<Value, String> {
    let entries: Vec<MyWordsEntry> = serde_json::from_value(value.clone())
        .map_err(|e| format!("my_words.entries: not a valid entries array: {e}"))?;
    let sanitized = sanitize_loaded_snapshot(MyWordsSnapshot {
        enabled: true,
        entries,
    });
    serde_json::to_value(sanitized.entries)
        .map_err(|e| format!("my_words.entries: serialize sanitized entries: {e}"))
}

/// Drop empty/whitespace-only triggers, trim whitespace, and collapse
/// repeated internal whitespace in triggers (so the user's typed
/// "hello  world" matches the compiled-side single-space form, which the
/// pattern builder turns into `\s+`). Replacement is preserved verbatim.
fn sanitize_snapshot(snap: MyWordsSnapshot) -> MyWordsSnapshot {
    let entries = snap
        .entries
        .into_iter()
        .filter_map(|e| {
            let trigger = collapse_internal_whitespace(e.trigger.trim());
            if trigger.is_empty() {
                return None;
            }
            Some(MyWordsEntry {
                trigger,
                replacement: e.replacement,
            })
        })
        .collect();
    MyWordsSnapshot {
        enabled: snap.enabled,
        entries,
    }
}

pub(crate) fn collapse_internal_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn compile_entries(entries: &[MyWordsEntry]) -> Vec<CompiledRule> {
    let mut compiled: Vec<CompiledRule> = entries.iter().filter_map(build_compiled_rule).collect();
    // Longest trigger first so longest-match-wins falls out of the
    // sort+greedy step.
    compiled.sort_by_key(|c| std::cmp::Reverse(c.trigger_len));
    compiled
}

fn build_compiled_rule(entry: &MyWordsEntry) -> Option<CompiledRule> {
    let trigger = entry.trigger.trim();
    if trigger.is_empty() {
        return None;
    }

    // Trigger may contain `|`-separated alternates (e.g.
    // "slash cpm|slush cpm|slash c p m"). Build a per-alternate
    // sub-pattern with its own word-boundary anchors, then join with
    // `|` inside a non-capturing group. A single-alternate trigger
    // produces a regex byte-identical to the pre-alternation version.
    //
    // The `regex` crate matches alternation with leftmost-first (Perl-
    // like) semantics, not POSIX longest-match: given "cpm|cpm pro", it
    // tries "cpm" first and — because that alone already matches at the
    // same starting position — never gets to try "cpm pro", even though
    // the input is "cpm pro". Sorting alternates longest-first at
    // compile time (by the alternate's own text length, before
    // word-boundary/whitespace expansion) makes the longer phrase win
    // whenever both could start a match at the same position.
    let mut alt_entries: Vec<(usize, String)> = trigger
        .split('|')
        .filter_map(|alt| {
            let alt_len = alt.trim().chars().count();
            build_alternate_pattern(alt).map(|pattern| (alt_len, pattern))
        })
        .collect();
    if alt_entries.is_empty() {
        return None;
    }
    alt_entries.sort_by_key(|(len, _)| std::cmp::Reverse(*len));
    let alternates: Vec<String> = alt_entries
        .into_iter()
        .map(|(_, pattern)| pattern)
        .collect();

    let pattern = if alternates.len() == 1 {
        format!("(?i){}", alternates[0])
    } else {
        format!("(?i)(?:{})", alternates.join("|"))
    };

    // Tiebreak length for the overlap-resolution sort: the longest
    // alternate. Longer-trigger-wins still falls out naturally.
    let trigger_len = trigger
        .split('|')
        .map(|alt| alt.trim().len())
        .max()
        .unwrap_or(trigger.len());

    match Regex::new(&pattern) {
        Ok(regex) => Some(CompiledRule {
            regex,
            replacement: entry.replacement.clone(),
            trigger_len,
        }),
        Err(err) => {
            log::warn!(
                target: "my_words",
                "skipping rule with uncompilable pattern {pattern:?}: {err}"
            );
            None
        }
    }
}

/// Compile a single alternate phrase (no `|`) into its word-boundary-
/// anchored, whitespace-tolerant sub-pattern. Returns `None` for an
/// empty/whitespace-only alternate so an accidental "|" at the edge
/// or doubled `||` doesn't poison the joined regex.
fn build_alternate_pattern(alt: &str) -> Option<String> {
    let trimmed = alt.trim();
    if trimmed.is_empty() {
        return None;
    }
    let shards: Vec<String> = trimmed
        .split_whitespace()
        .map(|word| relax_apostrophe_class(&regex::escape(word)))
        .collect();
    if shards.is_empty() {
        return None;
    }
    // Gap between trigger words: whitespace, ASCII hyphen, or the en/em
    // dash codepoints (U+2013/U+2014). ASR commonly returns compound
    // terms hyphenated ("plan feature" → "plan-feature"), and some
    // providers/autocorrect substitute the Unicode dash forms instead of
    // a plain hyphen; accepting all of them as a separator keeps the
    // user's spoken trigger firing without forcing them to enumerate
    // every hyphenation variant.
    let core = shards.join("[-\u{2013}\u{2014}\\s]+");

    let starts_with_word = trimmed
        .chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false);
    let ends_with_word = trimmed
        .chars()
        .next_back()
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false);

    let leading = if starts_with_word { r"\b" } else { "" };
    let trailing = if ends_with_word { r"\b" } else { "" };
    Some(format!("{leading}{core}{trailing}"))
}

/// Character class matching any of the three apostrophe / single-quote
/// codepoints we routinely see at the trigger-vs-input boundary:
/// `'` (U+0027 APOSTROPHE) — what Deepgram and most ASR backends
/// return in contractions ("let's", "don't"); `'` (U+2018 LEFT SINGLE
/// QUOTATION MARK) and `'` (U+2019 RIGHT SINGLE QUOTATION MARK) —
/// what macOS auto-substitutes typed `'` into in WebKit text fields
/// when "Use smart quotes and dashes" is enabled (the default), so the
/// React My Words editor stores a user-typed `let's` as `let's`.
/// Without this relaxation the compiled regex would literally require
/// the curly form and never match Deepgram's straight-quote output.
const APOSTROPHE_CLASS: &str = "[\u{0027}\u{2018}\u{2019}]";

/// Post-process a `regex::escape`-d shard: any literal apostrophe-
/// family codepoint becomes [`APOSTROPHE_CLASS`] so the compiled trigger
/// matches input regardless of which apostrophe shape it carries.
/// `regex::escape` does NOT escape these codepoints (none are regex
/// metacharacters), so scanning the escaped string for them and
/// expanding inline is safe — we never touch a backslash-escaped
/// metacharacter.
fn relax_apostrophe_class(escaped: &str) -> String {
    if !escaped
        .chars()
        .any(|c| matches!(c, '\u{0027}' | '\u{2018}' | '\u{2019}'))
    {
        return escaped.to_string();
    }
    let mut out = String::with_capacity(escaped.len() + APOSTROPHE_CLASS.len());
    for c in escaped.chars() {
        match c {
            '\u{0027}' | '\u{2018}' | '\u{2019}' => out.push_str(APOSTROPHE_CLASS),
            other => out.push(other),
        }
    }
    out
}

/// Public helper for callers that have raw JSON in hand (e.g. settings
/// migration paths). Currently unused but kept symmetric with the
/// `serialize_value` path the IPC layer would otherwise have to inline.
#[allow(dead_code)]
pub fn snapshot_to_value(snap: &MyWordsSnapshot) -> Value {
    json!({
        "enabled": snap.enabled,
        "entries": snap.entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mw(entries: Vec<(&str, &str)>) -> MyWords {
        MyWords::from_snapshot(MyWordsSnapshot {
            enabled: true,
            entries: entries
                .into_iter()
                .map(|(t, r)| MyWordsEntry {
                    trigger: t.into(),
                    replacement: r.into(),
                })
                .collect(),
        })
    }

    #[test]
    fn case_insensitive_match() {
        let m = mw(vec![("cloud code", "Claude Code")]);
        assert_eq!(
            m.apply("i wrote in cloud code today"),
            "i wrote in Claude Code today"
        );
        assert_eq!(m.apply("CLOUD CODE rules"), "Claude Code rules");
    }

    #[test]
    fn word_boundary_does_not_match_inside_word() {
        let m = mw(vec![("cat", "feline")]);
        assert_eq!(m.apply("I love concatenate"), "I love concatenate");
        assert_eq!(m.apply("a cat sat"), "a feline sat");
    }

    #[test]
    fn whitespace_tolerance() {
        let m = mw(vec![("hello world", "HW")]);
        assert_eq!(m.apply("hello   world"), "HW");
        assert_eq!(m.apply("hello\tworld"), "HW");
        assert_eq!(m.apply("hello\nworld"), "HW");
    }

    #[test]
    fn longest_match_wins() {
        let m = mw(vec![("foo bar", "FB"), ("foo", "F")]);
        assert_eq!(m.apply("foo bar baz"), "FB baz");
    }

    #[test]
    fn mid_sentence_substitution() {
        let m = mw(vec![("slash plan feature", "/plan-feature")]);
        assert_eq!(
            m.apply("let's slash plan feature this"),
            "let's /plan-feature this"
        );
    }

    #[test]
    fn no_cascade_after_substitution() {
        // a→b then b→c on "a" must NOT produce "c" — rules apply to the
        // ORIGINAL text only.
        let m = mw(vec![("a", "b"), ("b", "c")]);
        assert_eq!(m.apply("a"), "b");
        // And the b→c rule still works on its own when "b" is in the input.
        assert_eq!(m.apply("a and b"), "b and c");
    }

    #[test]
    fn kill_switch_off() {
        let m = MyWords::from_snapshot(MyWordsSnapshot {
            enabled: false,
            entries: vec![MyWordsEntry {
                trigger: "foo".into(),
                replacement: "BAR".into(),
            }],
        });
        assert_eq!(m.apply("foo foo foo"), "foo foo foo");
    }

    #[test]
    fn replacement_verbatim_casing() {
        let m = mw(vec![("hello", "Hello, World!")]);
        assert_eq!(m.apply("HELLO friend"), "Hello, World! friend");
    }

    #[test]
    fn replacement_with_special_chars_literal() {
        // $1 must not expand as a capture-group backref.
        let m = mw(vec![("dollar one", "$1.99")]);
        assert_eq!(m.apply("the dollar one menu"), "the $1.99 menu");
    }

    #[test]
    fn replacement_with_slash() {
        let m = mw(vec![("slash plan feature", "/plan-feature")]);
        assert_eq!(
            m.apply("run slash plan feature now"),
            "run /plan-feature now"
        );
    }

    #[test]
    fn empty_input_returns_empty() {
        let m = mw(vec![("foo", "bar")]);
        assert_eq!(m.apply(""), "");
    }

    #[test]
    fn no_rules_returns_input_unchanged() {
        let m = mw(vec![]);
        assert_eq!(m.apply("anything goes"), "anything goes");
    }

    #[test]
    fn multiple_non_overlapping_matches() {
        let m = mw(vec![("foo", "F"), ("bar", "B")]);
        assert_eq!(m.apply("foo and bar and foo"), "F and B and F");
    }

    #[test]
    fn sanitize_drops_empty_triggers() {
        let snap = sanitize_snapshot(MyWordsSnapshot {
            enabled: true,
            entries: vec![
                MyWordsEntry {
                    trigger: "  ".into(),
                    replacement: "x".into(),
                },
                MyWordsEntry {
                    trigger: "ok".into(),
                    replacement: "OK".into(),
                },
                MyWordsEntry {
                    trigger: "".into(),
                    replacement: "y".into(),
                },
            ],
        });
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].trigger, "ok");
    }

    #[test]
    fn sanitize_collapses_repeated_whitespace_in_trigger() {
        let snap = sanitize_snapshot(MyWordsSnapshot {
            enabled: true,
            entries: vec![MyWordsEntry {
                trigger: "  hello   world  ".into(),
                replacement: "HW".into(),
            }],
        });
        assert_eq!(snap.entries[0].trigger, "hello world");
    }

    #[test]
    fn unicode_trigger_round_trips() {
        let m = mw(vec![("café", "Café Latte")]);
        assert_eq!(m.apply("a café please"), "a Café Latte please");
    }

    #[test]
    fn bare_word_alternate_fires_in_mid_sentence() {
        // Regression for the brainstorm case: rule trigger is
        // "slash brainstorm|brainstorm" — the bare-word alternate must
        // match "brainstorm" inside a real sentence even when the
        // "slash" alternate doesn't apply.
        let m = mw(vec![("slash brainstorm|brainstorm", "/brainstorm")]);
        assert_eq!(
            m.apply("let's brainstorm about this"),
            "let's /brainstorm about this"
        );
        assert_eq!(
            m.apply("slash brainstorm about this"),
            "/brainstorm about this"
        );
    }

    #[test]
    fn trigger_words_match_across_ascii_hyphen() {
        // ASR variance: a multi-word compound may come back hyphenated
        // ("plan feature" → "plan-feature"). The trigger's word-gap
        // accepts whitespace AND hyphen so the rule still fires.
        let m = mw(vec![("plan feature", "/plan-feature")]);
        assert_eq!(
            m.apply("let's plan feature this"),
            "let's /plan-feature this"
        );
        assert_eq!(
            m.apply("let's plan-feature this"),
            "let's /plan-feature this"
        );
        assert_eq!(
            m.apply("let's plan - feature this"),
            "let's /plan-feature this"
        );
    }

    #[test]
    fn pipe_alternation_matches_either_phrase() {
        let m = mw(vec![("slash cpm|slush cpm", "/cpm")]);
        assert_eq!(m.apply("slash cpm now"), "/cpm now");
        assert_eq!(m.apply("slush cpm now"), "/cpm now");
    }

    #[test]
    fn pipe_alternation_handles_spaced_letters_variant() {
        // ASR variance: sometimes "cpm" comes back as "c p m".
        // One rule can absorb both shapes.
        let m = mw(vec![("slash cpm|slash c p m", "/cpm")]);
        assert_eq!(m.apply("slash cpm"), "/cpm");
        assert_eq!(m.apply("slash c p m"), "/cpm");
        assert_eq!(m.apply("slash c  p  m"), "/cpm"); // whitespace-tolerant per alt
    }

    #[test]
    fn pipe_alternation_tolerates_whitespace_around_separator() {
        let m = mw(vec![("slash cpm | slush cpm", "/cpm")]);
        assert_eq!(m.apply("slush cpm"), "/cpm");
    }

    #[test]
    fn pipe_alternation_drops_empty_alternates() {
        // Stray leading/trailing/doubled `|` shouldn't poison the
        // joined regex — empties get filtered.
        let m = mw(vec![("|slash cpm||slush cpm|", "/cpm")]);
        assert_eq!(m.apply("slash cpm"), "/cpm");
        assert_eq!(m.apply("slush cpm"), "/cpm");
    }

    #[test]
    fn pipe_alternation_preserves_word_boundaries_per_alt() {
        // Each alternate must still anchor at word boundaries — so a
        // trigger like "slash cpm" doesn't fire inside "slashcpms".
        let m = mw(vec![("slash cpm|slush cpm", "/cpm")]);
        assert_eq!(m.apply("slashcpms today"), "slashcpms today");
        assert_eq!(m.apply("a slash cpm"), "a /cpm");
    }

    #[test]
    fn pipe_alternation_no_pipe_is_byte_identical_to_legacy() {
        // Regression: a trigger without `|` must keep firing exactly as
        // before — same word-boundary behavior, same overlap precedence.
        let m = mw(vec![("slash plan feature", "/plan-feature")]);
        assert_eq!(
            m.apply("let's slash plan feature this"),
            "let's /plan-feature this"
        );
    }

    #[test]
    fn pipe_alternation_longest_alt_governs_overlap_tiebreak() {
        // When two rules' matches overlap on the same start, the rule
        // with the longer trigger wins. With alternation, the *longest*
        // alternate is the rule's representative length.
        // "slash plan feature" (18 chars) should beat "slash" (5 chars).
        let m = mw(vec![
            ("slash|x", "X"),
            ("slash plan feature|slush plan feature", "/plan-feature"),
        ]);
        assert_eq!(m.apply("slash plan feature now"), "/plan-feature now");
    }

    #[test]
    fn overlapping_distinct_starts_earlier_wins() {
        // Two rules whose matches overlap at different starts —
        // greedy-left wins. Document the contract.
        let m = mw(vec![("foo bar", "FB"), ("bar baz", "BZ")]);
        // "foo bar baz" — both rules match, overlap on "bar".
        // Greedy-left accepts "foo bar" first, leaving " baz".
        assert_eq!(m.apply("foo bar baz"), "FB baz");
    }

    #[test]
    fn curly_apostrophe_trigger_matches_straight_apostrophe_input() {
        // Regression for the 2026-05-14 dogfood failure: macOS smart-
        // quotes auto-substituted typed `'` in the React My Words
        // editor to U+2019 (`’`), so the stored trigger looked like
        // `let’s park` while Deepgram returns `let's park` with the
        // ASCII apostrophe U+0027. Without the apostrophe-class
        // relaxation in `build_alternate_pattern`, the compiled regex
        // literal-matched only the curly form and never fired.
        let m = mw(vec![("let\u{2019}s park", "let\u{2019}s /park")]);
        // ASR-shape input: straight ASCII apostrophe.
        assert_eq!(
            m.apply("let's park this for later"),
            "let\u{2019}s /park this for later",
            "trigger with U+2019 must match U+0027 input"
        );
    }

    #[test]
    fn straight_apostrophe_trigger_matches_curly_input() {
        // Symmetric case: ASCII-apostrophe trigger should also match
        // smart-quoted input (paranoia — Deepgram doesn't emit curly
        // today but other ASR backends or future versions might).
        let m = mw(vec![("let's park", "let's /park")]);
        assert_eq!(
            m.apply("let\u{2019}s park this for later"),
            "let's /park this for later",
            "trigger with U+0027 must match U+2019 input"
        );
    }

    #[test]
    fn apostrophe_class_covers_left_single_quote_too() {
        // U+2018 LEFT SINGLE QUOTATION MARK shows up rarely (opening
        // quote rather than apostrophe), but macOS smart-substitution
        // can produce it in edge cases. Lock the symmetry.
        let m = mw(vec![("\u{2018}quoted\u{2019}", "marked")]);
        for input in [
            "see 'quoted' here",
            "see \u{2018}quoted\u{2019} here",
            "see \u{2018}quoted' here",
        ] {
            assert_eq!(
                m.apply(input),
                input
                    .replace("'quoted'", "marked")
                    .replace("\u{2018}quoted\u{2019}", "marked")
                    .replace("\u{2018}quoted'", "marked"),
                "input {input:?} should normalise across apostrophe shapes"
            );
        }
    }

    #[test]
    fn relax_apostrophe_class_is_noop_when_no_apostrophe_present() {
        // Hot-path guard: shards without apostrophe chars must be
        // returned without allocation churn. This is observable only
        // via the function output (allocation behaviour is a perf
        // detail), so we just verify the byte-level no-op.
        let escaped = regex::escape("plain word");
        assert_eq!(relax_apostrophe_class(&escaped), escaped);
    }

    #[test]
    fn relax_apostrophe_class_expands_all_three_codepoints() {
        // Each of U+0027 / U+2018 / U+2019 must expand to the shared
        // character class, not just one of them — otherwise the
        // user's stored trigger-side curly form still wouldn't match
        // their dictated straight form (the original bug).
        for input in ["'", "\u{2018}", "\u{2019}"] {
            let escaped = regex::escape(input);
            let relaxed = relax_apostrophe_class(&escaped);
            assert_eq!(
                relaxed, APOSTROPHE_CLASS,
                "codepoint {input:?} should expand to {APOSTROPHE_CLASS}"
            );
        }
    }

    // --- Plan 039 / Slice 7 / Task 23 regressions ---------------------

    #[test]
    fn alternates_sorted_longest_first_so_prefix_does_not_shadow_longer_alt() {
        // "cpm" is a leftmost-first match at the same starting position
        // as "cpm pro" — without sorting alternates longest-first at
        // compile time, the bare "cpm" alternate wins and " pro" is left
        // dangling in the output instead of the full phrase firing.
        let m = mw(vec![("cpm|cpm pro", "CPM")]);
        assert_eq!(m.apply("run cpm pro now"), "run CPM now");
        // The bare alternate must still fire on its own when the longer
        // phrase isn't present.
        assert_eq!(m.apply("run cpm now"), "run CPM now");
    }

    #[test]
    fn overlap_tiebreak_uses_matched_span_not_declared_alternate_len() {
        // Rule B declares a long alternate that never matches this
        // input, inflating its compile-time trigger_len far past rule
        // A's. The winner must be decided by what actually matched in
        // this text (span length), not the rule's longest-declared
        // alternate — otherwise rule B's short "cpm" match would beat
        // rule A's genuinely longer "cpm pro" match.
        let m = mw(vec![
            ("cpm pro", "CPM_PRO"),
            ("cpm|cpm unused long alternate that never matches", "CPM"),
        ]);
        assert_eq!(m.apply("run cpm pro now"), "run CPM_PRO now");
    }

    #[test]
    fn word_gap_matches_en_and_em_dash() {
        let m = mw(vec![("plan feature", "/plan-feature")]);
        assert_eq!(
            m.apply("let's plan\u{2013}feature this"),
            "let's /plan-feature this",
            "en dash (U+2013) should bridge trigger words"
        );
        assert_eq!(
            m.apply("let's plan\u{2014}feature this"),
            "let's /plan-feature this",
            "em dash (U+2014) should bridge trigger words"
        );
    }

    #[test]
    fn load_time_cap_truncates_oversized_snapshot() {
        let entries: Vec<MyWordsEntry> = (0..MAX_ENTRIES + 50)
            .map(|i| MyWordsEntry {
                trigger: format!("trigger{i}"),
                replacement: format!("replacement{i}"),
            })
            .collect();
        let snap = sanitize_loaded_snapshot(MyWordsSnapshot {
            enabled: true,
            entries,
        });
        assert_eq!(snap.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn load_time_cap_is_noop_under_cap() {
        let entries: Vec<MyWordsEntry> = (0..10)
            .map(|i| MyWordsEntry {
                trigger: format!("trigger{i}"),
                replacement: format!("replacement{i}"),
            })
            .collect();
        let snap = sanitize_loaded_snapshot(MyWordsSnapshot {
            enabled: true,
            entries: entries.clone(),
        });
        assert_eq!(snap.entries.len(), entries.len());
    }
}

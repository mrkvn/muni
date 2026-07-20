//! Feature 029 — Runtime self-correction marker cleanup.
//!
//! Applied between ASR finalization and the Groq cleanup pass (in
//! `session.rs::cleanup_then_paste`), this module deterministically
//! removes self-correction markers (`scratch that`, `actually no`,
//! `I mean`, etc.) and the cancelled portion of the user's thought
//! BEFORE the LLM sees the text. The LLM then does its remaining
//! jobs (filler removal, grammar, punctuation, capitalization,
//! list/URL handling) on a marker-free input.
//!
//! ## Algorithm
//!
//! Markers come in two strengths. **Explicit** markers (`scratch that`,
//! `never mind`, `let me restart`, and compositions like `actually
//! scratch that` / `oh never mind` / `actually wait`) always act. **Weak**
//! markers (bare `wait`) occur constantly as ordinary speech ("we should
//! WAIT for the results"), so they act ONLY when a genuine restart is
//! detected and are otherwise left verbatim — a weak marker never
//! full-cancels.
//!
//! Markers are resolved right-to-left (the last restart wins). For the
//! rightmost marker that actually resolves to an edit:
//!
//! 1. Locate the marker phrase (which can include surrounding filler
//!    like `you know what`, `actually`, `oh`, `hmm`, `no`, plus a
//!    trailing comma).
//! 2. Look at the first word AFTER the marker (leading quotes /
//!    punctuation stripped).
//! 3. Find that word's RIGHTMOST whole-word occurrence in the PRE-marker
//!    text, bounding the deletion to the NEAREST restart. Whole-word
//!    matching means `we` does not match inside `we've`.
//! 4. Delete from that occurrence through the marker's end. Keep
//!    everything before it AND everything after the marker — the user is
//!    "restarting" from the matched word.
//! 5. No occurrence in the pre-marker text: an explicit marker
//!    full-cancels (drop everything before it); a weak marker is left
//!    verbatim. If the first word cannot be identified at all (the post
//!    is empty or all punctuation) never full-cancel — a dangling
//!    explicit marker is dropped, everything else is left verbatim.
//!
//! ## Known limitation — paraphrase restarts on unpunctuated input
//!
//! The ASR runs with `punctuate=false`, so this pass sees NO sentence
//! boundaries — only a flat word stream. The word-overlap heuristic
//! therefore bounds a restart well only when the correction RE-SAYS a word
//! from the clause it replaces (`meet at five … scratch that meet at six` →
//! overlap on `meet`). When the correction instead PARAPHRASES the scratched
//! clause with a different lead-in, the only shared token can be a bare
//! determiner: `the launch went well we got good feedback scratch that the
//! feedback was mixed` — the correction opens with `the`, whose sole
//! occurrence is back at `THE launch`, so step 3 anchors there and the whole
//! lead-in is dropped, collapsing to `the feedback was mixed`. There is no
//! reliable non-semantic signal for the true clause boundary in a
//! punctuation-free stream, and moving the decision back into the LLM
//! reintroduces the variance documented below. Accepted as a known limitation
//! (plan 039 dogfood 2026-07-09, QA scenario 2 step 3); the pinning test is
//! `paraphrase_restart_without_lexical_overlap_full_cancels`. A future,
//! separately-reviewed change may add determiner-skip + content-word anchoring
//! — see `.claude/backlogs.md`.
//!
//! ## Why runtime, not prompt
//!
//! The previous attempt (feat/029 prompt rewrite) instructed the LLM
//! to do the overlap-detection algorithm itself. Dogfood on
//! `gpt-oss-120b effort=medium` produced 50% reliability — the model
//! inconsistently chose to execute the algorithm vs. take the
//! shortcut of "drop everything before". Lifting the algorithm out
//! of the prompt and into deterministic Rust eliminates that
//! variance.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

/// Weak (ambiguous) marker phrases, in normalised form (see
/// [`normalize_marker`]). Unlike the explicit `scratch that`-class
/// markers these surface constantly as ordinary speech — "we should WAIT
/// for the results", the Taglish "wait lang" ("just a moment") — so they
/// fire ONLY when the post-marker text genuinely re-overlaps the
/// pre-marker text (a real restart). With no overlap they are left
/// verbatim and NEVER full-cancel, so a stray `wait` can't amputate the
/// user's lead-in. Only *bare* `wait` is weak; compositions like
/// `wait scratch that` or `actually wait` keep full strength. See plan
/// 039 slice 2 (`.claude/feature_plans/039_full_audit_remediation.md`).
///
/// `actually no` / `no actually` were audited alongside `wait` and kept
/// at full strength: their pure-hiccup case is already handled upstream
/// by the prompt-level demotion (plan 029), and in this runtime pass they
/// only full-cancel when the restart genuinely has no overlap.
const WEAK_MARKERS: &[&str] = &["wait"];

/// Safety cap on marker-reduction passes. Even pathological inputs have a
/// small number of markers per press; this bounds the loop if a fired
/// edit somehow re-exposes a marker.
const MAX_MARKER_PASSES: usize = 32;

/// True for a separator that may sit between the kept prefix and a
/// marker phrase: trailing whitespace or the optional preceding comma.
fn is_prefix_separator(c: char) -> bool {
    c.is_whitespace() || c == ','
}

/// Self-correction marker phrases. Longest first — regex alternation
/// tries left-to-right, so listing longer phrases first ensures the
/// matcher absorbs `actually scratch that` as one phrase rather than
/// stopping at bare `scratch that`.
///
/// Each phrase is matched case-insensitively and tolerates `[,\s]+`
/// between words (so `actually, no` matches `actually no` and
/// `no, scratch that` matches `no scratch that`).
///
/// The set is intentionally hand-rolled rather than auto-generated
/// from a filler × marker cross-product — we want explicit control
/// over which compositions ARE markers (e.g. `actually wait` is a
/// composition, but `actually` alone is NOT a marker).
const MARKER_PHRASES: &[&str] = &[
    // 5-word compositions (filler+filler+marker)
    "you know what scratch that",
    "you know what never mind",
    "you know what nevermind",
    // "you know what i mean" intentionally EXCLUDED — it is an idiomatic
    // discourse tag ("you get me, right?"), the OPPOSITE of a restart, and
    // overwhelmingly used as conversational filler mid-thought. Treating it
    // as a marker triggered full cancellation that nuked the entire
    // preceding dictation (see the regression test below). Past-tense
    // "you know what i meant" stays — "i meant" leans correction, not tag.
    "you know what i meant",
    // 3-word
    "let me start over",
    "actually scratch that",
    "actually never mind",
    "actually nevermind",
    "actually i mean",
    "actually i meant",
    "actually wait",
    "no scratch that",
    "no never mind",
    "no nevermind",
    "no i mean",
    "no i meant",
    "wait scratch that",
    "wait never mind",
    "wait nevermind",
    "wait i mean",
    "wait i meant",
    "oh scratch that",
    "oh never mind",
    "oh nevermind",
    "oh i mean",
    "oh i meant",
    "hmm scratch that",
    "hmm never mind",
    "hmm nevermind",
    "hmm i mean",
    "hmm i meant",
    "let me restart",
    "let me rephrase",
    // 2-word
    "scratch that",
    "never mind",
    // Bare "i mean" / "i meant" intentionally EXCLUDED — they are too
    // ambiguous (often used as elaboration, not restart). Cascaded usage
    // like "what do you mean by X — i mean Y" would otherwise hit the
    // full-cancellation fallback and nuke the user's prior content. The
    // LLM handles bare "i mean" per the CleanupPrompt PRESERVE rule
    // (wraps in commas as voice). Compositions like `actually i mean`,
    // `no i mean`, `wait i mean`, `oh i mean`, and `hmm i mean` keep
    // firing — the leading hesitation filler is a strong restart signal.
    // `you know what i mean` does NOT (see the exclusion above): "you
    // know what" turns it into an idiomatic tag, not a restart.
    "actually no",
    "no actually",
    // `no wait` / `oh wait` / `hmm wait` — the SAME hesitation-filler + `wait`
    // compositions as `no scratch that` / `oh scratch that` above, kept at full
    // strength (plan 039 slice 2, finding 3). The leading `no`/`oh`/`hmm` is a
    // strong restart signal, unlike BARE `wait` (which is weak and never
    // full-cancels — "we should wait for the results"). Without these, demoting
    // bare `wait` also silently demoted these genuine restarts, so a no-overlap
    // correction ("meet Monday, no wait, Friday") would pass through with the
    // marker left verbatim in the text.
    "no wait",
    "oh wait",
    "hmm wait",
    // 1-word
    "nevermind",
    "wait",
];

/// The combined marker regex, compiled once. Anchored with `\b` on both ends
/// so `wait` doesn't fire inside `waiter` and `i mean` doesn't fire inside
/// `dimension`; the marker phrases use `[,\s]+` for internal gaps so commas
/// between filler and marker are tolerated. `LazyLock` (task 3c) keeps the hot
/// path from rebuilding it — the previous per-call `Regex::new` in the overlap
/// search is gone entirely (whole-word matching is now done by a non-regex
/// token scan; see [`word_position_rightmost`]).
static MARKER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    let alternatives: Vec<String> = MARKER_PHRASES
        .iter()
        .map(|phrase| {
            phrase
                .split(' ')
                .map(regex::escape)
                .collect::<Vec<_>>()
                .join(r"[,\s]+")
        })
        .collect();
    // Optional trailing comma after the marker phrase — absorbed so it
    // doesn't survive into the output's pre-marker tail.
    let pattern = format!(r"(?i)\b(?:{})\b,?", alternatives.join("|"));
    RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .expect("self-correction marker regex must compile")
});

fn marker_regex() -> &'static Regex {
    &MARKER_REGEX
}

/// Reduce a raw token to its comparable core by trimming boundary
/// non-alphanumeric characters (quotes, brackets, punctuation, boundary
/// apostrophes) while keeping internal apostrophes.
fn token_core(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Strip a trailing English possessive marker (`'s` or a bare trailing
/// apostrophe) from an already-lowercased token core, returning the root
/// when one was present. This lets a restart word match its possessive
/// form in the pre-marker text — `friday` matches `friday's` — WITHOUT
/// loosening the whole-word rule that keeps `we` from matching `we've`:
/// the possessive suffix is anchored at the END, so `we've` (apostrophe
/// mid-token, ending in `ve`) is never reduced to `we`.
fn possessive_root(core_lc: &str) -> Option<&str> {
    core_lc
        .strip_suffix("'s")
        .or_else(|| core_lc.strip_suffix('\''))
}

/// Whether a raw haystack token, reduced to its [`token_core`] and
/// lowercased, matches the already-lowercased `needle` — either exactly
/// or via its possessive root (see [`possessive_root`]).
fn token_core_matches(raw_token: &str, needle_lc: &str) -> bool {
    let core_lc = token_core(raw_token).to_lowercase();
    core_lc == needle_lc || possessive_root(&core_lc) == Some(needle_lc)
}

/// Byte offset of the RIGHTMOST whole-word occurrence of `needle` in
/// `haystack`, compared case-insensitively (task 3a — nearest restart).
///
/// The haystack is tokenised EXACTLY as [`first_word`] reads the restart word:
/// split on whitespace, each token reduced to its [`token_core`] and compared
/// via [`token_core_matches`] (which also accepts a possessive form so `friday`
/// matches `friday's`). Sharing one tokenisation is what lets a restart word
/// carrying internal punctuation — a hyphenated compound (`co-worker`), a
/// decimal (`3.5`), a clock time (`3:30`) — find its pre-marker occurrence:
/// both sides keep it as one token instead of the haystack splitting on the
/// `-` / `.` / `:` while the needle stays whole. Boundary punctuation (a lone
/// `-` separator, a trailing period) is trimmed by [`token_core`], so it is
/// never mistaken for part of the word.
///
/// Whole-token comparison is deliberately stricter than the old `\bwe\b` regex,
/// which matched the `we` inside `we've`: with rightmost selection that false
/// sub-word match would cut at the contraction and duplicate content. Token
/// equality keeps `we` from matching `we've` (the possessive relaxation is
/// END-anchored, so a mid-token apostrophe never reduces `we've` to `we`).
fn word_position_rightmost(haystack: &str, needle: &str) -> Option<usize> {
    let needle_lc = needle.to_lowercase();
    if needle_lc.is_empty() {
        return None;
    }
    let mut best: Option<usize> = None;
    let mut token_start: Option<usize> = None;
    let mut token_end = 0usize;
    for (i, c) in haystack.char_indices() {
        if c.is_whitespace() {
            if let Some(start) = token_start.take() {
                if token_core_matches(&haystack[start..token_end], &needle_lc) {
                    best = Some(start);
                }
            }
        } else {
            if token_start.is_none() {
                token_start = Some(i);
            }
            token_end = i + c.len_utf8();
        }
    }
    if let Some(start) = token_start {
        if token_core_matches(&haystack[start..token_end], &needle_lc) {
            best = Some(start);
        }
    }
    best
}

/// Byte offset within `post` of the first whitespace-delimited token whose
/// [`token_core`] is non-empty — i.e. the start of the restart word.
///
/// This skips any leading whitespace AND any leading pure-punctuation
/// separator tokens (a stray period, dash, or ellipsis the ASR emitted
/// between the marker and the restart — `scratch that. the feedback…`,
/// `scratch that - send…`, `scratch that… meet…`). The marker regex only
/// absorbs an optional trailing comma, so every other separator arrives
/// here as post-text and must not be mistaken for the restart word.
///
/// Punctuation GLUED to the word (a leading quote, `"friday`) is preserved:
/// that token's core is the word itself, so its offset is returned and the
/// quote survives into the stitched suffix. Returns `None` when the post
/// has no word-bearing token (empty or all punctuation).
fn restart_word_offset(post: &str) -> Option<usize> {
    let mut token_start: Option<usize> = None;
    for (i, c) in post.char_indices() {
        if c.is_whitespace() {
            if let Some(start) = token_start.take() {
                if !token_core(&post[start..i]).is_empty() {
                    return Some(start);
                }
            }
        } else if token_start.is_none() {
            token_start = Some(i);
        }
    }
    if let Some(start) = token_start {
        if !token_core(&post[start..]).is_empty() {
            return Some(start);
        }
    }
    None
}

/// The post-marker text starting at the restart word — leading whitespace
/// and pure-punctuation separator tokens removed, punctuation glued to the
/// word preserved (see [`restart_word_offset`]). Empty when the post has no
/// word-bearing token.
fn restart_suffix(post: &str) -> &str {
    match restart_word_offset(post) {
        Some(off) => &post[off..],
        None => "",
    }
}

/// Extract the first "word" after a marker — the [`token_core`] of the
/// restart token (task 3b: boundary non-word characters stripped, so a
/// quoted first word like `"friday` reduces to `friday`). Internal
/// apostrophes are kept so `I'll` stays whole. Leading separator tokens are
/// skipped via [`restart_suffix`].
///
/// Returns `None` when there is no identifiable word (post empty or all
/// punctuation), which the caller treats as no-overlap — never a
/// full-cancel.
fn first_word(text: &str) -> Option<String> {
    let token = restart_suffix(text).split_whitespace().next()?;
    let core = token_core(token);
    if core.is_empty() {
        None
    } else {
        Some(core.to_string())
    }
}

/// Normalise a matched marker span to a canonical `word word` form so it
/// can be looked up in [`WEAK_MARKERS`]: lowercased, internal `[,\s]+`
/// runs collapsed to a single space, leading/trailing separators dropped.
/// `Wait,` and ` wait ` both normalise to `wait`; `actually wait` stays
/// `actually wait`.
fn normalize_marker(matched: &str) -> String {
    let mut out = String::with_capacity(matched.len());
    let mut pending_space = false;
    for c in matched.chars() {
        if is_prefix_separator(c) {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Whether the matched marker span is a weak (ambiguous) marker that must
/// not full-cancel on a no-overlap occurrence. See [`WEAK_MARKERS`].
fn is_weak_marker(matched: &str) -> bool {
    WEAK_MARKERS.contains(&normalize_marker(matched).as_str())
}

/// Resolve a single marker occurrence. Returns:
/// - `Some(new_text)` when the marker fires (an edit was made), or
/// - `None` when the marker must be LEFT VERBATIM — a weak marker with no
///   restart overlap, or a marker whose restart word can't be identified
///   at all (never destroy content when the restart is unlocatable).
fn try_apply_at(text: &str, marker_start: usize, marker_end: usize) -> Option<String> {
    let pre = &text[..marker_start];
    let post = &text[marker_end..];
    let matched = &text[marker_start..marker_end];
    let weak = is_weak_marker(matched);

    let Some(first) = first_word(post) else {
        if post.trim().is_empty() {
            // Marker dangles at the end of the input. A weak marker (bare
            // `wait`) is left verbatim — there is no restart to act on. An
            // explicit marker (`scratch that`, …) drops itself and keeps
            // the pre-marker text.
            if weak {
                return None;
            }
            return Some(pre.trim_end_matches(is_prefix_separator).to_string());
        }
        // Post has content but no identifiable restart word (all
        // punctuation / symbols). We cannot locate a restart, so we must
        // NOT full-cancel and destroy the pre-marker content — leave the
        // text verbatim regardless of marker strength (task 3b).
        return None;
    };

    // The kept post-marker text begins at the restart word — any leading
    // separator punctuation (period/dash/ellipsis the ASR emitted after the
    // marker) is dropped so it doesn't survive into the stitched output.
    let suffix = restart_suffix(post);

    match word_position_rightmost(pre, &first) {
        Some(cut_from) => {
            // Restart detected: the user re-said `first`, so delete from
            // its NEAREST (rightmost) pre-marker occurrence through the
            // marker.
            let prefix_trimmed = pre[..cut_from].trim_end_matches(is_prefix_separator);
            if prefix_trimmed.is_empty() {
                Some(suffix.to_string())
            } else {
                // Single space between kept prefix and suffix so the LLM
                // sees normal word spacing.
                Some(format!("{} {}", prefix_trimmed, suffix))
            }
        }
        None => {
            // No overlap. A weak marker with no restart is a false positive
            // ("we should wait for the results") — leave it verbatim.
            if weak {
                return None;
            }
            // Explicit marker, no overlap → full cancellation.
            Some(suffix.to_string())
        }
    }
}

/// Apply runtime self-correction cleanup. If no markers are present,
/// returns the input unchanged (zero-copy via [`Cow::Borrowed`]).
///
/// Multi-marker chains are reduced right-to-left so the last restart
/// wins (matches the LLM examples in `CleanupPrompt.md` for nested
/// constructions like `I mean Thursday no wait Friday`).
pub fn apply(input: &str) -> Cow<'_, str> {
    let regex = marker_regex();
    if regex.find(input).is_none() {
        return Cow::Borrowed(input);
    }

    let mut text = input.to_string();
    let mut mutated = false;
    for _ in 0..MAX_MARKER_PASSES {
        // Collect the marker spans in the CURRENT text as owned offsets so
        // no borrow of `text` is held across the mutation below.
        let spans: Vec<(usize, usize)> = regex
            .find_iter(&text)
            .map(|m| (m.start(), m.end()))
            .filter(|(s, e)| e > s)
            .collect();
        if spans.is_empty() {
            break;
        }
        // Fire the RIGHTMOST marker that actually resolves to an edit. A
        // weak marker that finds no restart is skipped (left verbatim) so
        // a marker further left still gets its chance; if none fires we're
        // done.
        let mut fired = false;
        for (start, end) in spans.iter().rev() {
            if let Some(next) = try_apply_at(&text, *start, *end) {
                text = next;
                mutated = true;
                fired = true;
                break;
            }
        }
        if !fired {
            break;
        }
    }

    if mutated {
        Cow::Owned(text)
    } else {
        // Every marker was left verbatim — the text is byte-identical to
        // the input, so preserve the zero-copy borrow.
        Cow::Borrowed(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SENTINEL 1 — restart-at-start (full cancel).
    /// Scenario 1 from `docs/qa/029_*.md`. First word after marker is
    /// `we`, leftmost `we` is right after `I think`, so kept prefix is
    /// `I think`.
    #[test]
    fn restart_at_start_collapses_to_post_marker_with_thin_prefix() {
        let input = "I think we should ship the change tomorrow because we've tested it thoroughly, actually no, we should ship it tomorrow because we've tested it thoroughly enough";
        let got = apply(input);
        assert_eq!(
            got.as_ref(),
            "I think we should ship it tomorrow because we've tested it thoroughly enough"
        );
    }

    /// SENTINEL 2 — mid-clause overlap (prefix preserved). This is the
    /// feat/028 dogfood reproduction. First word after marker is
    /// `rushing`, leftmost `rushing` is in `because rushing it last time`,
    /// so the `we should hold off...because ` prefix survives.
    #[test]
    fn mid_clause_overlap_preserves_long_prefix() {
        let input = "we should hold off on the migration until we get more data from the canary group because rushing it last time clearly didn't work out actually no rushing it last time clearly didn't work out then let's go to the market later";
        let got = apply(input);
        assert_eq!(
            got.as_ref(),
            "we should hold off on the migration until we get more data from the canary group because rushing it last time clearly didn't work out then let's go to the market later"
        );
    }

    /// Canonical `actually + scratch that` composition — position-0
    /// overlap on `move`, so full cancellation.
    #[test]
    fn actually_scratch_that_with_position_zero_overlap_full_cancels() {
        let input = "move the meeting to tuesday actually scratch that move it to wednesday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "move it to wednesday");
    }

    /// `no, scratch that` composition — position-0 overlap on `send`.
    /// Verifies the comma between filler and marker is tolerated.
    #[test]
    fn no_scratch_that_with_comma_full_cancels() {
        let input = "send the report by friday, no scratch that, send it by monday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "send it by monday");
    }

    /// Bare `actually` (not a marker) must be preserved verbatim.
    #[test]
    fn bare_actually_is_not_a_marker() {
        let input = "let's actually invoke this";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// Bare `i mean` is NOT a marker — the input is returned unchanged
    /// and the LLM handles the hedge per the CleanupPrompt PRESERVE rule.
    /// Pairs with the regression test below: cascaded bare `i mean` used
    /// as elaboration must NEVER trigger full cancellation.
    #[test]
    fn bare_i_mean_is_not_a_marker() {
        let input = "tell him that I'll be there by five I mean by six because I have a call at four thirty";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// REGRESSION — cascaded bare `i mean` used as elaboration (not
    /// restart). Previously the second `i mean` fired with first word
    /// `if` not appearing in the pre-text, triggering the full-cancel
    /// fallback and nuking everything before. Now bare `i mean` is not
    /// a marker, so the input passes through untouched and the LLM
    /// preserves all content per the PRESERVE rule.
    #[test]
    fn cascaded_bare_i_mean_does_not_truncate_prior_content() {
        let input = "what's the difference between basic and pro and also you said the api is free what do you mean by this i mean i know the api is paid i mean if you exceed the quota you get charged";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// REGRESSION (dev dogfood 2026-06-15) — "you know what i mean" used
    /// as an idiomatic discourse tag mid-thought must NOT trigger a
    /// restart. Previously it fired as a marker; the first word after
    /// ("maybe") had no overlap in the pre-text, so the full-cancellation
    /// fallback nuked the entire preceding dictation and only the tail
    /// survived. Now it is not a marker, so the input passes through whole.
    #[test]
    fn you_know_what_i_mean_tag_does_not_truncate_prior_content() {
        let input = "ensure that it can seamlessly fit there you know what i mean maybe put some fade at the bottom";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// `send it to alice no actually send it to bob` — first word after
    /// `no actually` = `send`, position 0 → full cancellation.
    #[test]
    fn no_actually_with_position_zero_full_cancels() {
        let input = "send it to alice no actually send it to bob";
        let got = apply(input);
        assert_eq!(got.as_ref(), "send it to bob");
    }

    /// `the report is due friday I mean thursday no wait friday` — only ONE
    /// marker actually fires here: bare `i mean` is not a marker, and bare
    /// `wait` is weak, but `no wait` is a full-strength composition (plan 039
    /// slice 2, finding 3). It resolves in a single edit: first word after the
    /// marker is `friday`, whose RIGHTMOST whole-word occurrence in the
    /// pre-marker text (`…is due friday I mean thursday`) sits right after
    /// `due`, so deletion is bounded to that nearest restart and the lead-in
    /// `the report is due` survives — never a full cancel of the whole clause.
    #[test]
    fn no_wait_composition_restarts_at_nearest_overlap() {
        let input = "the report is due friday I mean thursday no wait friday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "the report is due friday");
    }

    /// Plan 039 slice 2 (task 2) — bare `wait` is now a WEAK marker. With
    /// no restart overlap (first word after = `let's`, absent from the
    /// pre-marker text) it must NOT full-cancel; the whole utterance is
    /// left verbatim so a stray `wait` can't amputate the lead-in. (This
    /// test previously pinned the old full-cancel behavior.)
    #[test]
    fn standalone_wait_without_overlap_passes_through_verbatim() {
        let input = "we should go on tuesday wait let's go on wednesday";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// Acceptance criterion (slice 2) — ordinary speech using `wait` as a
    /// verb passes through untouched (no restart overlap → weak marker
    /// left verbatim), including the zero-copy borrow.
    #[test]
    fn bare_wait_as_ordinary_speech_passes_through() {
        let input = "we should wait for the results before deciding";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
        assert!(matches!(got, Cow::Borrowed(_)));
    }

    /// Task 3(b) — "wait for me": bare `wait` with no overlap is a false
    /// positive and is left verbatim rather than full-cancelling the
    /// lead-in.
    #[test]
    fn bare_wait_for_me_passes_through() {
        let input = "can you wait for me at the entrance";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// Task 3 — Taglish "wait lang" ("just a moment") is not a restart;
    /// the weak `wait` (here at the very start, empty pre-marker text)
    /// leaves the utterance untouched.
    #[test]
    fn taglish_wait_lang_passes_through() {
        let input = "wait lang tapos tayo aalis";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// Task 3 — bare `wait` DOES still fire when a genuine restart overlap
    /// is present (weak markers act on real restarts, they are only inert
    /// on no-overlap occurrences).
    #[test]
    fn bare_wait_with_overlap_still_restarts() {
        let input = "meet me at five wait meet me at six";
        let got = apply(input);
        assert_eq!(got.as_ref(), "meet me at six");
    }

    /// Task 3(a) — multi-sentence restart bounds deletion to the NEAREST
    /// (rightmost) occurrence of the restart word. `the` appears in two
    /// earlier sentences; only the last one is cut, so sentence 1
    /// survives.
    #[test]
    fn multi_sentence_scratch_that_restarts_at_nearest_occurrence() {
        let input =
            "the launch went well. the feedback was positive. scratch that, the feedback was mixed";
        let got = apply(input);
        assert_eq!(got.as_ref(), "the launch went well. the feedback was mixed");
    }

    /// Task 3(a) regression — rightmost matching must be WHOLE-WORD. The
    /// restart word `we` appears once as a real word and once inside the
    /// contraction `we've`; picking the contraction would duplicate
    /// content. Whole-word token matching keeps the correct restart point.
    #[test]
    fn rightmost_overlap_ignores_contraction_substring() {
        let input =
            "I think we should ship the change because we've tested it, actually no we should ship it now";
        let got = apply(input);
        assert_eq!(got.as_ref(), "I think we should ship it now");
    }

    /// Task 3(b) — the first word after the marker is quoted. Stripping
    /// the leading quote lets the overlap search find the restart at
    /// `friday` instead of mis-firing a full cancellation.
    #[test]
    fn quoted_first_word_after_marker_is_matched() {
        let input = "the report is due friday scratch that \"friday\" is the wrong date";
        let got = apply(input);
        assert_eq!(
            got.as_ref(),
            "the report is due \"friday\" is the wrong date"
        );
    }

    /// Task 3 — `never mind the X`: `never mind` stays a full-strength
    /// marker; the post-marker `the flight` overlaps the pre-marker text
    /// so it restarts at the nearest occurrence.
    #[test]
    fn never_mind_the_x_restarts_at_overlap() {
        let input = "book the flight for monday never mind the flight is already booked";
        let got = apply(input);
        assert_eq!(got.as_ref(), "book the flight is already booked");
    }

    /// `let me restart` triggers full cancellation.
    #[test]
    fn let_me_restart_full_cancels_when_no_overlap() {
        let input = "I was thinking about the proposal let me restart we should focus on the budget instead";
        let got = apply(input);
        assert_eq!(got.as_ref(), "we should focus on the budget instead");
    }

    /// Empty input is a no-op.
    #[test]
    fn empty_input_returns_empty() {
        let got = apply("");
        assert_eq!(got.as_ref(), "");
    }

    /// Input with NO marker is returned borrowed (no allocation).
    #[test]
    fn no_marker_returns_borrowed() {
        let input = "this is a perfectly normal sentence with no corrections";
        let got = apply(input);
        assert!(matches!(got, Cow::Borrowed(_)));
        assert_eq!(got.as_ref(), input);
    }

    /// Tagalog passthrough: bare `actually` in a Taglish utterance is
    /// preserved. No marker fires.
    #[test]
    fn taglish_with_bare_actually_preserved() {
        let input = "actually I think mas okay yung first option kasi simpler siya";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// Marker at the very end of the input (no post-marker content) —
    /// the marker phrase is dropped, the pre-marker text is preserved.
    #[test]
    fn marker_at_end_drops_marker_keeps_pre() {
        let input = "go home and rest actually no";
        let got = apply(input);
        assert_eq!(got.as_ref(), "go home and rest");
    }

    /// Marker doesn't fire when buried inside a longer word — `\b`
    /// boundary on both ends prevents `waiter` from being read as `wait`
    /// + `er`.
    #[test]
    fn marker_does_not_fire_inside_a_longer_word() {
        let input = "the waiter brought our food quickly";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// `nevermind` (one-word variant) fires the same as `never mind`.
    #[test]
    fn nevermind_one_word_variant_fires() {
        let input = "let's go to the park nevermind let's stay home";
        let got = apply(input);
        assert_eq!(got.as_ref(), "let's stay home");
    }

    /// Comma absorption: a trailing comma after the marker is removed so
    /// it doesn't survive into the stitched output as `prefix , post`.
    #[test]
    fn trailing_comma_after_marker_is_absorbed() {
        let input = "I'll bring the laptop actually no, I'll bring the tablet";
        let got = apply(input);
        // First word after marker = "I'll" (with apostrophe), leftmost
        // match in pre = position 0 ("I'll bring..."). Full cancel.
        assert_eq!(got.as_ref(), "I'll bring the tablet");
    }

    /// Capitalization: marker matches are case-insensitive.
    #[test]
    fn marker_match_is_case_insensitive() {
        let input = "let's meet Tuesday Actually No, let's meet Wednesday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "let's meet Wednesday");
    }

    /// Plan 039 slice 2 (finding 1) — an explicit `scratch that` followed by
    /// a PERIOD (not the comma the regex absorbs) must still fire. The
    /// leading `. ` is trimmed before the first word is read, so the restart
    /// word `the` is found in the pre-marker text and the nearest sentence
    /// is replaced instead of leaving the marker verbatim.
    #[test]
    fn explicit_marker_followed_by_period_still_fires() {
        let input =
            "the launch went well. the feedback was positive. scratch that. the feedback was mixed.";
        let got = apply(input);
        assert_eq!(
            got.as_ref(),
            "the launch went well. the feedback was mixed."
        );
    }

    /// Finding 1 — a dash separator after the marker is trimmed like any
    /// other leading non-word punctuation, so the restart word is located.
    #[test]
    fn explicit_marker_followed_by_dash_still_fires() {
        let input = "send the report by friday scratch that - send the report by monday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "send the report by monday");
    }

    /// Finding 1 — an ellipsis after the marker is trimmed; the restart word
    /// `meet` re-overlaps and the utterance restarts at the nearest one.
    #[test]
    fn explicit_marker_followed_by_ellipsis_still_fires() {
        let input = "meet me at five scratch that... meet me at six";
        let got = apply(input);
        assert_eq!(got.as_ref(), "meet me at six");
    }

    /// Finding 1 — a dangling explicit marker whose post-text is ALL
    /// punctuation has no locatable restart word, so it is left verbatim
    /// (never full-cancels the pre-marker content).
    #[test]
    fn explicit_marker_with_all_punctuation_post_is_left_verbatim() {
        let input = "go home and rest scratch that ...";
        let got = apply(input);
        assert_eq!(got.as_ref(), input);
    }

    /// Plan 039 slice 2 (finding 3) — the restart word matches its
    /// POSSESSIVE form in the pre-marker text: `friday` overlaps `friday's`,
    /// so deletion is bounded to that nearest occurrence and the lead-in is
    /// preserved rather than destroyed by an over-eager full cancel.
    #[test]
    fn restart_word_matches_possessive_form_in_pre_text() {
        let input = "the meeting is on friday's agenda scratch that friday works better";
        let got = apply(input);
        assert_eq!(got.as_ref(), "the meeting is on friday works better");
    }

    /// Finding 3 — the possessive relaxation stays END-anchored: `we` must
    /// still NOT match the contraction `we've`. The genuine `we` token is
    /// the correct (and only) restart point.
    #[test]
    fn contraction_still_not_matched_by_possessive_relaxation() {
        let input =
            "I think we should ship the change because we've tested it, actually no we should ship it now";
        let got = apply(input);
        assert_eq!(got.as_ref(), "I think we should ship it now");
    }

    /// Plan 039 slice 2 (finding 3) — the `no wait` composition is FULL
    /// strength (unlike bare weak `wait`): on a genuine restart with NO
    /// overlap it full-cancels the cancelled lead-in instead of leaving the
    /// marker verbatim in the pasted text. `friday` is absent from
    /// `meet monday`, so the utterance restarts from the marker's tail.
    #[test]
    fn no_wait_composition_full_cancels_when_no_overlap() {
        let input = "meet monday no wait friday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "friday");
    }

    /// Finding 3 — `no wait` with a restart overlap bounds deletion to the
    /// nearest occurrence of the restart word (`meet` at position 0 → the
    /// whole cancelled clause is replaced).
    #[test]
    fn no_wait_composition_restarts_on_overlap() {
        let input = "meet me on monday no wait meet me on friday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "meet me on friday");
    }

    /// Finding 3 — `oh wait` is the same full-strength composition and
    /// restarts at the nearest overlap of the restart word `call`.
    #[test]
    fn oh_wait_composition_restarts_on_overlap() {
        let input = "call alice at noon oh wait call alice at two";
        let got = apply(input);
        assert_eq!(got.as_ref(), "call alice at two");
    }

    /// Finding 3 — `hmm wait` is a full-strength composition; a no-overlap
    /// restart full-cancels rather than surviving verbatim.
    #[test]
    fn hmm_wait_composition_full_cancels_when_no_overlap() {
        let input = "ship it tuesday hmm wait wednesday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "wednesday");
    }

    /// Plan 039 round-2 finding (slice 2) — a HYPHENATED restart word must match
    /// its pre-marker occurrence. The restart word `co-worker` is read as one
    /// whitespace-delimited token, and the pre-marker haystack scan tokenises the
    /// same way (see [`word_position_rightmost`]). Before the shared tokenisation
    /// the scan split `co-worker` on the hyphen, the overlap search missed, and
    /// the explicit `scratch that` marker over-deleted — dropping the whole
    /// lead-in. Now deletion is BOUNDED to the nearest occurrence of the
    /// compound, so `sync with my` survives.
    #[test]
    fn hyphenated_restart_word_bounds_explicit_marker_deletion() {
        let input = "sync with my co-worker tomorrow scratch that co-worker meeting is friday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "sync with my co-worker meeting is friday");
    }

    /// Round-2 finding (slice 2) — the weak `wait` marker still fires on a
    /// genuine restart even when the restart word is hyphenated. Overlap on
    /// `co-worker` is found, so the cancelled clause is replaced rather than the
    /// marker being left verbatim.
    #[test]
    fn hyphenated_restart_word_still_fires_weak_wait() {
        let input = "meet my co-worker at five wait co-worker at six";
        let got = apply(input);
        assert_eq!(got.as_ref(), "meet my co-worker at six");
    }

    /// Round-2 finding (slice 2) — a boundary/standalone hyphen the ASR emits as
    /// a separator after the marker is still trimmed (its [`token_core`] is
    /// empty), so the hyphenated-word support does not cause a lone `-` to be
    /// mistaken for the restart word.
    #[test]
    fn lone_dash_separator_is_not_read_as_hyphenated_restart_word() {
        let input = "email the co-worker on monday scratch that - email the co-worker on friday";
        let got = apply(input);
        assert_eq!(got.as_ref(), "email the co-worker on friday");
    }

    /// Plan 039 round-2 finding (slice 2) — a restart word with an internal
    /// PERIOD (a decimal) must match its pre-marker occurrence. Because the
    /// haystack is tokenised on whitespace exactly as the restart word is read,
    /// `3.5` stays one token on both sides; the explicit `scratch that` bounds
    /// deletion to the nearest occurrence instead of full-cancelling the lead-in.
    /// Before the shared tokenisation the scan split `3.5` on the period, the
    /// overlap missed, and `the budget is` was destroyed.
    #[test]
    fn decimal_restart_word_bounds_explicit_marker_deletion() {
        let input = "the budget is 3.5 million scratch that 3.5 billion";
        let got = apply(input);
        assert_eq!(got.as_ref(), "the budget is 3.5 billion");
    }

    /// Round-2 finding (slice 2) — a restart word with an internal COLON (a
    /// clock time) matches the same way; `3:30` is not split on the colon, so
    /// the lead-in `meet at` survives instead of being full-cancelled.
    #[test]
    fn clock_time_restart_word_bounds_explicit_marker_deletion() {
        let input = "meet at 3:30 today scratch that 3:30 tomorrow";
        let got = apply(input);
        assert_eq!(got.as_ref(), "meet at 3:30 tomorrow");
    }

    /// REALISTIC-INPUT companion to
    /// [`multi_sentence_scratch_that_restarts_at_nearest_occurrence`] (which
    /// uses periods the `punctuate=false` ASR never emits). With the SAME shape
    /// but no punctuation, the nearest-occurrence heuristic still bounds
    /// correctly BECAUSE both later clauses open with `the`: the rightmost
    /// `the` is the near one, so only the middle clause is replaced.
    #[test]
    fn multi_clause_shared_determiner_restarts_at_nearest_unpunctuated() {
        let input =
            "the launch went well the feedback was positive scratch that the feedback was mixed";
        let got = apply(input);
        assert_eq!(got.as_ref(), "the launch went well the feedback was mixed");
    }

    /// KNOWN LIMITATION (plan 039 dogfood 2026-07-09, QA scenario 2 step 3) — pins the
    /// current behavior for a PARAPHRASE restart on realistic unpunctuated
    /// input. The scratched clause `we got good feedback` shares no word with
    /// the correction except `feedback`, and the correction opens with the
    /// determiner `the`, whose only occurrence is back at `THE launch`. With no
    /// sentence boundaries in the stream there is no reliable non-semantic clue
    /// that the corrected clause begins at `we got good feedback`, so the
    /// deletion over-reaches and collapses to the correction alone. Documented
    /// in the module header; see `.claude/backlogs.md` for the deferred
    /// determiner-skip follow-up. This test PINS the limitation so a future fix
    /// flips it deliberately rather than silently.
    #[test]
    fn paraphrase_restart_without_lexical_overlap_full_cancels() {
        let input = "the launch went well we got good feedback scratch that the feedback was mixed";
        let got = apply(input);
        // Ideal (needs semantics): "the launch went well the feedback was mixed".
        // Actual known limitation:
        assert_eq!(got.as_ref(), "the feedback was mixed");
    }
}

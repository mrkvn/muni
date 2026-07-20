//! macOS text-injection backend.
//!
//! Mirrors Swift v1's `Core/TextInjector.swift`. The strategy is unchanged:
//!
//! 1. Pre-flight: refuse if Accessibility is not granted.
//! 2. Snapshot the user's pasteboard so we can restore it after the paste
//!    lands.
//! 3. Write the cleaned text (with a single trailing space appended — see
//!    [`paste_payload`]) plus the `org.nspasteboard.TransientType` marker so
//!    well-behaved clipboard managers ignore the temporary write.
//! 4. Post a synthetic Cmd+V — directly to the focused app's process via
//!    `CGEventPostToPid` (see [`post_cmd_v`]) so a modifier-flag latch left by the
//!    re-paste hotkey chord can't corrupt it into ⌃⌘V; falls back to a session-tap
//!    post when the target pid is unknown.
//! 5. Sleep `paste_delay_ms` plus a length-adaptive restore delay (see
//!    [`effective_restore_delay_ms`]) so the receiving app has a chance to
//!    consume the paste before we put the user's clipboard back. The restore
//!    is purely time-based: macOS exposes no reliable "the pasteboard was
//!    read" signal, so the window must simply be wide enough that a briefly
//!    stalled target app still reads our text and not the restored clipboard.
//! 6. Restore the original pasteboard contents.
//!
//! Secure Input is *not* preflight-checked. macOS's `IsSecureEventInputEnabled`
//! is unreliable as a paste-feasibility probe — `loginwindow` regularly holds
//! it stuck after lock/unlock cycles even though synthetic Cmd+V from an
//! Accessibility-trusted process still lands. Empirically, Wispr Flow pastes
//! into password fields under this state; we match that behavior and let the
//! OS decide whether the paste actually applies.
//!
//! Threading notes
//! ---------------
//! `NSPasteboard` is on Apple's documented list of thread-safe AppKit classes,
//! and `CGEvent::post` / `AXIsProcessTrusted` are callable from any thread. We
//! therefore drive paste from a tokio worker thread without dispatching to the
//! main run loop. The Swift code's `@MainActor` annotation was actor-system
//! bookkeeping, not a hard main-thread requirement.

use async_trait::async_trait;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use objc2::rc::{autoreleasepool, Retained};
use objc2_app_kit::{NSPasteboard, NSPasteboardType, NSPasteboardTypeString};
use objc2_foundation::{NSData, NSString};

use super::{FocusProbe, PlatformInjector};
use crate::error::MuniError;

/// `kVK_ANSI_V` from `Carbon.HIToolbox`. The numeric literal is stable across
/// macOS versions; we prefer the constant over linking to Carbon just to read
/// a single value.
const KEY_V: CGKeyCode = 9;

/// `kVK_Return` from `Carbon.HIToolbox` — the main-keyboard Return key.
/// Posting this after a paste submits the focused field (sends the chat
/// message, etc.). Numpad Enter (`kVK_ANSI_KeypadEnter`) is a distinct
/// keycode, but for *injecting* a submit we always post the canonical
/// Return — apps treat the two identically once delivered.
const KEY_RETURN: CGKeyCode = 0x24;

/// Default delay between posting Cmd+V and the next sleep. Mirrors Swift v1.
///
/// This is the intrinsic default used by [`MacInjector::default`] (and tests).
/// The runtime value comes from the user-configurable `paste.delay_ms` setting,
/// which defaults to this same 50 ms — a drift-guard test
/// (`settings_default_matches_injector_default`) pins the two together.
pub const DEFAULT_PASTE_DELAY_MS: u64 = 50;

/// Upper bound applied to the configured `paste.delay_ms` before it reaches the
/// injector. A persisted setting is external input; clamping keeps a corrupt or
/// pathological value from stalling every paste (Priority #1: speed). 1 s is far
/// beyond any legitimate paste delay while still finite.
pub const MAX_PASTE_DELAY_MS: u64 = 1000;

/// Base delay before restoring the user's pasteboard.
///
/// Deliberately *wider* than Swift v1's 120 ms. A rare race motivated the
/// bump: if the frontmost app's main thread is stalled when our synthetic
/// Cmd+V arrives, it can read the pasteboard *after* we've already restored
/// the user's previous clipboard — so it pastes that stale content instead of
/// the dictation. The restore runs after the paste is already on screen, so
/// widening this window costs no user-perceived latency (Priority #1); it only
/// delays when the original clipboard returns. See [`effective_restore_delay_ms`]
/// for the length-adaptive extension layered on top of this base.
pub const DEFAULT_RESTORE_DELAY_MS: u64 = 350;

/// Length scaling for the restore delay: 1 ms of extra window per this many
/// characters of pasted text. A larger payload takes the receiving app
/// fractionally longer to read off the pasteboard, so the window grows with
/// size — bounded by [`MAX_RESTORE_DELAY_MS`].
const RESTORE_DELAY_CHARS_PER_MS: u64 = 10;

/// Hard ceiling on the restore delay (base + length scaling). Keeps a
/// pathologically large paste from stalling the clipboard restore — and any
/// auto-submit Enter that runs after it — for an unreasonable time.
const MAX_RESTORE_DELAY_MS: u64 = 800;

/// Pasteboard hint adopted by clipboard managers (Maccy, Paste, etc.) to mean
/// "do not record this entry in history". We write zero-byte data under this
/// type so the sequence-bumping write counts as transient.
const TRANSIENT_TYPE: &str = "org.nspasteboard.TransientType";

/// `kAXErrorSuccess` — an Accessibility call returned a usable value.
const AX_ERROR_SUCCESS: i32 = 0;

/// `kAXErrorAttributeUnsupported` — the element does not expose this attribute.
/// Used to tell a native element (no `AXDOMClassList`) from web content (every
/// WebKit/Chromium element supports it), so the focus probe never mistakes an
/// editable web surface for a confident "nothing to type into".
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25205;

/// Accessibility roles that denote a text-entry element. A focused element with
/// one of these is treated as editable even if the value-settable probe is
/// inconclusive (belt-and-suspenders bias toward pasting; Feature 037 task 5).
const EDITABLE_AX_ROLES: [&str; 4] = ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"];

/// Roles that are confidently NOT text-editable, regardless of app or whether
/// the element is native or web. A focused element with one of these means
/// "nothing to type into" even when the AXValue-settable probe is inconclusive
/// (web controls report a read-only value indistinguishably from an editable
/// canvas). This is what lets the no-field notice fire for a non-editable focus
/// inside a web/Electron/Tauri app — e.g. a focused `AXButton`.
///
/// Container/document roles (`AXGroup`, `AXWebArea`, `AXScrollArea`, `AXOutline`)
/// are DELIBERATELY excluded: they can host an editable canvas — Google Docs'
/// editor focuses an `AXGroup` while reporting a read-only value — so they stay
/// on the paste-biased path. Never suppress a paste on an ambiguous container.
const NON_EDITABLE_AX_ROLES: [&str; 9] = [
    "AXButton",
    "AXStaticText",
    "AXLink",
    "AXImage",
    "AXCheckBox",
    "AXRadioButton",
    "AXMenuItem",
    "AXPopUpButton",
    "AXDisclosureTriangle",
];

/// Upper bound on a *single* focus-probe AX round-trip, in seconds. A wedged
/// frontmost app must never stall paste delivery (speed is priority 1); on
/// timeout the AX call returns an error and the probe yields `Unknown` → paste
/// as today. Far below Apple's 6 s default. Note this bounds each round-trip
/// individually: `focused_field_probe` makes up to four of them, so the
/// worst-case *synchronous* cost is ~4× this — which is why the whole probe is
/// additionally capped by [`FOCUS_PROBE_BUDGET`] off the runtime (see
/// [`probe_with_budget`]).
const FOCUS_PROBE_TIMEOUT_SECS: f32 = 0.25;

/// Overall wall-clock budget for the focus probe, enforced *outside* the
/// blocking AX calls. [`FOCUS_PROBE_TIMEOUT_SECS`] caps each individual AX
/// round-trip, but [`focused_field_probe`] makes up to four of them, so a fully
/// wedged frontmost app could still burn ~1 s inside one synchronous probe.
/// This deadline caps the *whole* probe: past it we stop waiting and paste
/// (fail-open), holding the wedged-app tax on paste latency to ~this bound
/// rather than the ~1 s worst case. 300 ms leaves comfortable headroom for a
/// healthy 4-round-trip probe (each well under 250 ms in practice) while
/// staying imperceptible next to the paste+restore window.
const FOCUS_PROBE_BUDGET: Duration = Duration::from_millis(300);

/// macOS injector. Configurable delays let tests dial paste/restore down to
/// near-zero without changing production timing.
pub struct MacInjector {
    pub paste_delay_ms: u64,
    pub restore_delay_ms: u64,
}

impl Default for MacInjector {
    fn default() -> Self {
        Self {
            paste_delay_ms: DEFAULT_PASTE_DELAY_MS,
            restore_delay_ms: DEFAULT_RESTORE_DELAY_MS,
        }
    }
}

#[async_trait]
impl PlatformInjector for MacInjector {
    async fn paste(&self, text: &str) -> Result<(), MuniError> {
        if text.is_empty() {
            return Err(MuniError::NothingToPaste);
        }
        if !is_accessibility_granted() {
            return Err(MuniError::AccessibilityDenied);
        }

        // Snapshot + write happen synchronously inside an autorelease pool so
        // the temporary `NSString` / `NSArray` allocations are released before
        // the surrounding async sleep.
        let snapshot = autoreleasepool(|_| {
            let pb = general_pasteboard();
            let snapshot = snapshot_pasteboard(&pb);
            write_to_pasteboard(&pb, &paste_payload(text));
            snapshot
        });

        // The re-paste hotkey can share its key with the synthetic paste — the
        // default chord is ⌃⌘V and we inject ⌘V. macOS coalesces (drops) a
        // synthetic V key-down issued while the user is still physically holding
        // V, so the paste silently no-ops. Wait — bounded — for V to lift first.
        // On the normal dictation path V is never held, so `is_key_physically_down`
        // is false on the first poll and this returns immediately (no added
        // latency on the hot path).
        wait_for_key_release(KEY_V).await;

        // Post the synthetic ⌘V DIRECTLY to the focused app's process
        // (`CGEventPostToPid`), NOT into the shared event stream. The re-paste
        // chord (⌃⌘V) leaves Control+Command latched in the WindowServer modifier-
        // flags state even after the physical keys lift (observed during dogfood
        // 2026-07-09 as `CGEventSourceFlagsState == 0x00140109` while every
        // modifier key reported UP). An event injected at the HID or session tap is
        // re-flagged with that latch → the target sees ⌃⌘V, not a paste, and
        // nothing lands (the flaky "~1-in-20" re-paste). A per-pid post carries
        // exactly the Command flag we set, immune to the latch. `post_cmd_v` falls
        // back to a session-tap post when the pid can't be resolved.
        let target_pid = focused_app_pid();
        post_cmd_v(target_pid)?;

        tokio::time::sleep(Duration::from_millis(self.paste_delay_ms)).await;
        let restore_ms = effective_restore_delay_ms(self.restore_delay_ms, text.chars().count());
        tokio::time::sleep(Duration::from_millis(restore_ms)).await;

        autoreleasepool(|_| {
            let pb = general_pasteboard();
            restore_pasteboard(&pb, snapshot);
        });

        Ok(())
    }

    async fn press_enter(&self) -> Result<(), MuniError> {
        // Mirror the paste path's single pre-flight: posting a synthetic
        // key into another app requires the same Accessibility grant. In
        // practice this always holds here (we only reach `press_enter`
        // right after a paste that already passed the check), but the
        // guard keeps the method correct if it's ever called standalone.
        if !is_accessibility_granted() {
            return Err(MuniError::AccessibilityDenied);
        }
        post_return()
    }

    async fn has_editable_focus(&self) -> FocusProbe {
        // AX focus data is only meaningful (and only readable) when Muni is
        // Accessibility-trusted. Onboarding owns the first-time prompt; mid
        // session a missing grant is treated as doubt, never a confident
        // negative — so we paste exactly as today.
        if !is_accessibility_granted() {
            return FocusProbe::Unknown;
        }
        // `focused_field_probe` makes up to four *blocking* cross-process AX
        // round-trips (focused element → role → AXValue-settable → DOM class
        // list), each capped by `FOCUS_PROBE_TIMEOUT_SECS` — so a wedged
        // frontmost app can burn ~1 s of wall time inside that one synchronous
        // call. Two hazards follow: it must never run on an async runtime
        // worker (it would stall every other task sharing that thread), and
        // even off-thread it must never add more than a small fixed tax to
        // paste latency (speed is priority 1). So we hand it to
        // `spawn_blocking` and race it against `FOCUS_PROBE_BUDGET`; on timeout
        // (or a join failure) we fail *open* to `Unknown` — i.e. paste exactly
        // as the no-probe path did, because doubt must never swallow a paste.
        probe_with_budget(FOCUS_PROBE_BUDGET, focused_field_probe).await
    }
}

/// Pure role-based pre-classification, split from [`focused_field_probe`] so the
/// confident cases are unit-testable without a live AX round-trip. `Some` is a
/// conclusive verdict from the role alone; `None` means the role is inconclusive
/// (an ambiguous container/document such as `AXGroup`) and the caller must fall
/// back to the AXValue-settable + native/web heuristic.
fn classify_role(role: &str) -> Option<FocusProbe> {
    if EDITABLE_AX_ROLES.contains(&role) {
        Some(FocusProbe::Editable)
    } else if NON_EDITABLE_AX_ROLES.contains(&role) {
        Some(FocusProbe::NoEditableField)
    } else {
        None
    }
}

/// Run a blocking focus probe off the async runtime with an overall deadline.
///
/// The probe can make several cross-process Accessibility round-trips and block
/// for up to ~1 s against an unresponsive frontmost app, so running it inline on
/// a runtime worker would stall every other task pinned to that thread. It goes
/// to [`spawn_blocking`](tokio::task::spawn_blocking); racing that against
/// `budget` guarantees the paste path waits at most `budget` for a verdict. Any
/// non-verdict outcome — the deadline elapsing, or the blocking task
/// panicking/being cancelled — resolves to [`FocusProbe::Unknown`] (fail-open:
/// paste as today), because a swallowed paste is the worst possible failure.
///
/// Generic over the probe closure so the timeout/fail-open contract is unit
/// testable with a deterministic slow stub, without a live AX round-trip.
async fn probe_with_budget<F>(budget: Duration, probe: F) -> FocusProbe
where
    F: FnOnce() -> FocusProbe + Send + 'static,
{
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(probe)).await {
        Ok(Ok(verdict)) => verdict,
        Ok(Err(join_err)) => {
            log::warn!(target: "paste", "focus probe task failed ({join_err}) — pasting");
            FocusProbe::Unknown
        }
        Err(_elapsed) => {
            log::warn!(
                target: "paste",
                "focus probe exceeded {} ms budget — pasting",
                budget.as_millis()
            );
            FocusProbe::Unknown
        }
    }
}

/// Probe whether the frontmost app has an editable field focused, via up to four
/// system-wide Accessibility round-trips (focused element → role →
/// AXValue-settable → DOM class list; no tree walk). Blocking — callers must run
/// it off the async runtime under a deadline (see [`probe_with_budget`]).
///
/// Returns [`FocusProbe::NoEditableField`] **only** on a confident negative — a
/// focused **native** element whose value AX positively reports as read-only and
/// whose role is not a text-entry role. Every ambiguous outcome (no focused
/// element, any AX error/timeout, an unsupported/unreadable attribute, or **any
/// web content** — Chrome/Safari can report a non-settable `AXValue` on an
/// editable surface like Google Docs) yields [`FocusProbe::Unknown`] so the
/// caller pastes exactly as it does today. This asymmetry is deliberate and
/// load-bearing: a wrong `NoEditableField` would swallow a real paste — the
/// worst possible failure — so doubt always biases toward pasting.
fn focused_field_probe() -> FocusProbe {
    use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    use core_foundation_sys::base::Boolean;
    use core_foundation_sys::string::CFStringRef;

    // AXUIElementRef is an opaque CFType handle; aliasing it to CFTypeRef lets
    // the same pointer flow through CFRelease without casts.
    type AxUiElementRef = CFTypeRef;
    type AxError = i32;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateSystemWide() -> AxUiElementRef;
        fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout: f32) -> AxError;
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AxError;
        fn AXUIElementIsAttributeSettable(
            element: AxUiElementRef,
            attribute: CFStringRef,
            settable: *mut Boolean,
        ) -> AxError;
    }

    // SAFETY: `AXUIElementCreateSystemWide` yields a +1 CFType we own; the AX
    // reads are documented thread-safe. Every Create/Copy result is released on
    // every return path, and pointers are dereferenced only behind a success
    // return plus a non-null check.
    unsafe {
        let system_wide = AXUIElementCreateSystemWide();
        if system_wide.is_null() {
            return FocusProbe::Unknown;
        }
        // Bound the round-trip so an unresponsive frontmost app can't stall
        // delivery. Best-effort: a failure here just leaves Apple's default.
        let _ = AXUIElementSetMessagingTimeout(system_wide, FOCUS_PROBE_TIMEOUT_SECS);

        let focused_attr = CFString::from_static_string("AXFocusedUIElement");
        let mut focused: CFTypeRef = std::ptr::null();
        let copy_err = AXUIElementCopyAttributeValue(
            system_wide,
            focused_attr.as_concrete_TypeRef(),
            &mut focused,
        );
        // The system-wide handle is no longer needed once the copy has run.
        CFRelease(system_wide);

        if copy_err != AX_ERROR_SUCCESS || focused.is_null() {
            // No focused element, or AX couldn't tell us — doubt → paste.
            return FocusProbe::Unknown;
        }

        // `focused` is now a +1 CFType we own and must release on every path.
        // Positive signal: a text-entry role means editable regardless of the
        // value-settable probe (some editors report a non-settable AXValue).
        let role_attr = CFString::from_static_string("AXRole");
        let mut role_ref: CFTypeRef = std::ptr::null();
        let role_err =
            AXUIElementCopyAttributeValue(focused, role_attr.as_concrete_TypeRef(), &mut role_ref);
        let role_str = if role_err == AX_ERROR_SUCCESS && !role_ref.is_null() {
            // AXRole is documented to return a CFString; adopt the +1 so it is
            // released when the wrapper drops.
            CFString::wrap_under_create_rule(role_ref as CFStringRef).to_string()
        } else {
            String::new()
        };
        let probe = if let Some(resolved) = classify_role(&role_str) {
            // The role alone is conclusive — an editable text role, or a
            // confidently non-editable control (button, label, link, image…).
            resolved
        } else {
            // Ambiguous container/document role (AXGroup, AXWebArea, …). Fall
            // back to the AXValue-settable probe.
            // Primary editability signal: is the element's AXValue settable?
            let value_attr = CFString::from_static_string("AXValue");
            let mut settable: Boolean = 0;
            let settable_err = AXUIElementIsAttributeSettable(
                focused,
                value_attr.as_concrete_TypeRef(),
                &mut settable,
            );
            if settable_err != AX_ERROR_SUCCESS {
                // AXValue unsupported / the call couldn't complete → doubt.
                // Web areas, custom canvases, etc. land here and still paste.
                FocusProbe::Unknown
            } else if settable != 0 {
                FocusProbe::Editable
            } else {
                // AX reports a read-only AXValue. That is a confident negative
                // ONLY for a NATIVE element. Web engines (Chrome/Safari) hand
                // back a non-settable AXValue on genuinely editable surfaces —
                // Google Docs' canvas editor is the canonical example — so unless
                // we can CONFIRM this is native, bias to doubt and paste. Web AX
                // always exposes `AXDOMClassList`; native AX reports it
                // unsupported. Any other (ambiguous) error also biases to paste.
                let dom_attr = CFString::from_static_string("AXDOMClassList");
                let mut dom_value: CFTypeRef = std::ptr::null();
                let dom_err = AXUIElementCopyAttributeValue(
                    focused,
                    dom_attr.as_concrete_TypeRef(),
                    &mut dom_value,
                );
                if !dom_value.is_null() {
                    CFRelease(dom_value);
                }
                if dom_err == AX_ERROR_ATTRIBUTE_UNSUPPORTED {
                    // Confirmed native, read-only focus → the one confident
                    // negative (e.g. the Finder desktop).
                    FocusProbe::NoEditableField
                } else {
                    // Web content (or an ambiguous AX error). Editability is
                    // NOT reliably knowable here: Chrome reports a read-only
                    // AXValue AND exposes no `AXEditableAncestor` on canvas
                    // editors like Google Docs — indistinguishable from a static
                    // web label. Tried and rejected `AXEditableAncestor` (it
                    // mislabels Google Docs as non-editable, swallowing a real
                    // paste). Never-swallow-a-paste wins: bias to doubt and
                    // paste. Cost: no "held" notice for a non-editable web /
                    // Electron / Tauri focus — the dictation just blind-pastes,
                    // exactly as before this feature (never worse than status quo).
                    FocusProbe::Unknown
                }
            }
        };

        CFRelease(focused);
        probe
    }
}

fn general_pasteboard() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

/// Capture every type currently on the pasteboard along with its raw bytes.
///
/// Returning a `HashMap<String, Vec<u8>>` keyed by the UTI string deliberately
/// drops the Objective-C ownership story: by the time we restore, the original
/// `NSData` instances would be gone with the autorelease pool anyway, so we
/// keep an owned byte copy that's safe to ferry across the post-paste sleep.
fn snapshot_pasteboard(pb: &NSPasteboard) -> HashMap<String, Vec<u8>> {
    let mut snapshot = HashMap::new();
    // `types` returns nil when the pasteboard is empty.
    let Some(types) = pb.types() else {
        return snapshot;
    };

    let count = types.count();
    for i in 0..count {
        let ty: Retained<NSPasteboardType> = types.objectAtIndex(i);
        let Some(data) = pb.dataForType(&ty) else {
            continue;
        };
        let key = ty.to_string();
        let bytes = data.to_vec();
        snapshot.insert(key, bytes);
    }
    snapshot
}

/// Build the exact string that gets written to the pasteboard.
///
/// A single trailing space is appended so the user can keep typing (or
/// fire another dictation) immediately after a paste without manually
/// inserting a separator: `paste #1` + `paste #2` lands as
/// `"…dictation 1. …dictation 2. "` instead of `"…dictation 1.…dictation 2."`.
/// The transcript event and history pipeline both run on the un-padded
/// text — only the pasteboard payload carries the trailing space.
fn paste_payload(text: &str) -> String {
    format!("{text} ")
}

/// Compute the actual restore delay (ms) for a paste of `text_len` characters.
///
/// Starts from `base_ms` (the injector's [`MacInjector::restore_delay_ms`]) and
/// adds 1 ms per [`RESTORE_DELAY_CHARS_PER_MS`] characters, clamped to
/// [`MAX_RESTORE_DELAY_MS`]. Because it only ever *adds* to the base, the
/// restore window can widen with payload size but never shrink below the
/// configured floor — and that floor is the guarantee that prevents a stalled
/// target app from reading the pasteboard after an early restore (the
/// stale-clipboard race). Kept as a free function so it's unit-testable without
/// touching AppKit.
fn effective_restore_delay_ms(base_ms: u64, text_len: usize) -> u64 {
    let extra = text_len as u64 / RESTORE_DELAY_CHARS_PER_MS;
    base_ms.saturating_add(extra).min(MAX_RESTORE_DELAY_MS)
}

fn write_to_pasteboard(pb: &NSPasteboard, text: &str) {
    pb.clearContents();

    let ns_text = NSString::from_str(text);
    // SAFETY: `NSPasteboardTypeString` is a Foundation-defined extern static
    // (a stable UTI string constant) — reading it is sound on macOS.
    let pasteboard_string_type = unsafe { NSPasteboardTypeString };
    let _ = pb.setString_forType(&ns_text, pasteboard_string_type);

    let transient_type = transient_type();
    let empty = NSData::data();
    // Empty NSData under the transient hint tells clipboard managers
    // (Maccy/Paste/etc.) to skip this write — the cleaned text never lands
    // in the user's clipboard history.
    let _ = pb.setData_forType(Some(&empty), &transient_type);
}

fn restore_pasteboard(pb: &NSPasteboard, snapshot: HashMap<String, Vec<u8>>) {
    pb.clearContents();
    for (ty_str, bytes) in snapshot {
        let ty = pasteboard_type(&ty_str);
        let data = NSData::with_bytes(&bytes);
        let _ = pb.setData_forType(Some(&data), &ty);
    }
}

fn transient_type() -> Retained<NSPasteboardType> {
    pasteboard_type(TRANSIENT_TYPE)
}

fn pasteboard_type(ident: &str) -> Retained<NSPasteboardType> {
    // `NSPasteboardType` is a type alias for `NSString`; constructing one from
    // a Rust &str gives us the same shape AppKit expects.
    NSString::from_str(ident)
}

fn post_cmd_v(target_pid: Option<i32>) -> Result<(), MuniError> {
    use foreign_types::ForeignType;
    use std::os::raw::c_void;

    // SAFETY: `CGEventSource::new` / `CGEvent::new_keyboard_event` are thread-safe
    // and yield owned values; `CGEventPostToPid` takes a borrowed CGEventRef.
    //
    // The re-paste chord (⌃⌘V) leaves Control+Command LATCHED in the WindowServer
    // modifier-flags state even after the physical keys lift — observed during
    // dogfood 2026-07-09 as `CGEventSourceFlagsState == 0x00140109`
    // (Command|Control) while every modifier key reported UP. An event injected
    // into the shared event stream (HID or session tap) is RE-FLAGGED with that
    // latched state, overriding our `set_flags(Command)`, so our ⌘V arrives as
    // ⌃⌘V — not a paste — and nothing lands (AX read-back confirmed `grew=0`).
    // Posting DIRECTLY to the focused app's pid delivers the event to that
    // process's queue with exactly the flags we set, immune to the global latch.
    // The session-tap post is kept only as a fallback when the pid is unknown.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventPostToPid(pid: i32, event: *const c_void);
    }

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| MuniError::PasteInjectionFailed)?;

    let key_down = CGEvent::new_keyboard_event(source.clone(), KEY_V, true)
        .map_err(|()| MuniError::PasteInjectionFailed)?;
    key_down.set_flags(CGEventFlags::CGEventFlagCommand);

    let key_up = CGEvent::new_keyboard_event(source, KEY_V, false)
        .map_err(|()| MuniError::PasteInjectionFailed)?;
    key_up.set_flags(CGEventFlags::CGEventFlagCommand);

    match target_pid {
        Some(pid) => unsafe {
            CGEventPostToPid(pid, key_down.as_ptr() as *const c_void);
            CGEventPostToPid(pid, key_up.as_ptr() as *const c_void);
        },
        None => {
            key_down.post(CGEventTapLocation::Session);
            key_up.post(CGEventTapLocation::Session);
        }
    }

    Ok(())
}

/// Poll-wait (bounded) for a physical key to be released before we synthesize a
/// keystroke that reuses it. Injecting the same key the user is still holding
/// (e.g. the `V` in a `⌃⌘V` re-paste chord vs. our injected `⌘V`) gets coalesced
/// by macOS and the paste is lost. Bounded so a genuinely stuck key can never
/// hang the paste — on timeout we fall through and post anyway (no worse than
/// before the guard).
async fn wait_for_key_release(key: CGKeyCode) {
    const MAX_WAIT: Duration = Duration::from_millis(700);
    const POLL: Duration = Duration::from_millis(8);

    let start = Instant::now();
    while is_key_physically_down(key) {
        if start.elapsed() >= MAX_WAIT {
            log::debug!(
                target: "paste",
                "wait_for_key_release: key still down after {MAX_WAIT:?} — posting paste anyway"
            );
            break;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Process id of the AX-focused application, for posting the synthetic paste
/// directly to it via `CGEventPostToPid` (bypasses the WindowServer session-level
/// modifier merge that corrupts a HID/session-tap ⌘V into ⌃⌘V). `None` on any AX
/// error — the caller then falls back to a session-tap post.
fn focused_app_pid() -> Option<i32> {
    use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    use core_foundation_sys::string::CFStringRef;

    type AxUiElementRef = CFTypeRef;
    type AxError = i32;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateSystemWide() -> AxUiElementRef;
        fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout: f32) -> AxError;
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AxError;
        fn AXUIElementGetPid(element: AxUiElementRef, pid: *mut i32) -> AxError;
    }

    // SAFETY: mirrors `focused_field_probe` — each Create/Copy result is released
    // on every path; the pid is read only behind a success return.
    unsafe {
        let system_wide = AXUIElementCreateSystemWide();
        if system_wide.is_null() {
            return None;
        }
        let _ = AXUIElementSetMessagingTimeout(system_wide, FOCUS_PROBE_TIMEOUT_SECS);
        let attr = CFString::from_static_string("AXFocusedApplication");
        let mut app: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(system_wide, attr.as_concrete_TypeRef(), &mut app);
        CFRelease(system_wide);
        if err != AX_ERROR_SUCCESS || app.is_null() {
            return None;
        }
        let mut pid: i32 = 0;
        let perr = AXUIElementGetPid(app, &mut pid);
        CFRelease(app);
        if perr != AX_ERROR_SUCCESS || pid <= 0 {
            return None;
        }
        Some(pid)
    }
}

/// Read the live hardware state of a key via `CGEventSourceKeyState`. The safe
/// `core_graphics` wrapper doesn't expose it, so it's declared directly — the
/// symbol resolves through the ApplicationServices umbrella framework already
/// linked for the Accessibility FFI above.
fn is_key_physically_down(key: CGKeyCode) -> bool {
    use core_foundation_sys::base::Boolean;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, key: CGKeyCode) -> Boolean;
    }

    // `HIDSystemState` reflects the true hardware key state, not this app's
    // synthetic event stream — exactly what tells us the user is still holding V.
    unsafe { CGEventSourceKeyState(CGEventSourceStateID::HIDSystemState as i32, key) != 0 }
}

/// Post a bare Return keypress (KeyDown + KeyUp, no modifier flags) to the
/// HID event location. Used by [`MacInjector::press_enter`] to submit the
/// focused field after a paste.
fn post_return() -> Result<(), MuniError> {
    // SAFETY: same as `post_cmd_v` — these CoreGraphics calls are
    // thread-safe and yield owned values.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| MuniError::PasteInjectionFailed)?;

    // Explicitly force EMPTY modifier flags on both events. Synthetic
    // events posted via `HIDSystemState` otherwise inherit the system's
    // current modifier state at post time — and the Command flag from the
    // synthetic Cmd+V paste that runs immediately before this is still
    // latched, so a bare Return would arrive as **Cmd+Return** (which, in
    // Ghostty and others, toggles fullscreen instead of submitting).
    // `set_flags` overrides the inherited flags for the event.
    let key_down = CGEvent::new_keyboard_event(source.clone(), KEY_RETURN, true)
        .map_err(|()| MuniError::PasteInjectionFailed)?;
    key_down.set_flags(CGEventFlags::empty());
    key_down.post(CGEventTapLocation::HID);

    let key_up = CGEvent::new_keyboard_event(source, KEY_RETURN, false)
        .map_err(|()| MuniError::PasteInjectionFailed)?;
    key_up.set_flags(CGEventFlags::empty());
    key_up.post(CGEventTapLocation::HID);

    Ok(())
}

/// Returns true iff Muni is currently trusted for Accessibility (i.e. allowed
/// to drive system events). We use the prompt-less flavor on the paste path —
/// onboarding (Phase 9) is responsible for the first-time prompt; mid-session
/// we just refuse and surface a typed error.
fn is_accessibility_granted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    // SAFETY: thread-safe per Apple's Accessibility docs.
    unsafe { AXIsProcessTrusted() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feature 037 dogfood: the role-based classification is what lets the
    /// no-field notice fire inside web/Electron/Tauri apps. Editable text roles
    /// paste; confidently non-editable controls (a focused xmux agent-name
    /// `AXButton`, a label, a link) yield the notice; ambiguous containers
    /// (`AXGroup` — Google Docs' editable canvas — `AXWebArea`, the Finder
    /// desktop's `AXOutline`) stay inconclusive so the value/native probe
    /// decides and a real editable canvas is never suppressed.
    #[test]
    fn classify_role_splits_editable_control_and_container() {
        for r in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert_eq!(classify_role(r), Some(FocusProbe::Editable), "{r}");
        }
        for r in [
            "AXButton",
            "AXStaticText",
            "AXLink",
            "AXImage",
            "AXMenuItem",
        ] {
            assert_eq!(classify_role(r), Some(FocusProbe::NoEditableField), "{r}");
        }
        for r in [
            "AXGroup",
            "AXWebArea",
            "AXScrollArea",
            "AXOutline",
            "",
            "AXWhatever",
        ] {
            assert_eq!(classify_role(r), None, "{r} must stay inconclusive");
        }
    }

    /// Plan 039 slice 17: a wedged frontmost app can make `focused_field_probe`
    /// block for ~1 s (up to four AX round-trips). The overall budget must cap
    /// that: the probe is abandoned near the deadline and the paste proceeds
    /// (fail-open → `Unknown`), adding only ~budget latency, never the ~1 s
    /// worst case. Uses a stub that outlives the budget by design so the timeout
    /// path is exercised deterministically.
    #[tokio::test(flavor = "current_thread")]
    async fn slow_probe_fails_open_within_budget() {
        let budget = Duration::from_millis(300);
        let started = Instant::now();
        let verdict = probe_with_budget(budget, || {
            // Far longer than the budget so the deadline always wins.
            std::thread::sleep(Duration::from_secs(1));
            // Even a confident negative from the stub must be discarded on
            // timeout — a swallowed paste is the worst possible failure.
            FocusProbe::NoEditableField
        })
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            verdict,
            FocusProbe::Unknown,
            "timed-out probe must fail open to Unknown (paste proceeds)"
        );
        assert!(
            elapsed < budget + Duration::from_millis(50),
            "probe overran its budget: {elapsed:?} (budget {budget:?})"
        );
    }

    /// A probe that returns before the deadline must surface its real verdict —
    /// the budget only guards the wedged case, it must not blur a confident
    /// negative into `Unknown` and re-enable a blind paste.
    #[tokio::test(flavor = "current_thread")]
    async fn fast_probe_returns_real_verdict() {
        let verdict =
            probe_with_budget(Duration::from_millis(300), || FocusProbe::NoEditableField).await;
        assert_eq!(verdict, FocusProbe::NoEditableField);
    }

    /// A panicking probe (a defensive stand-in for any `spawn_blocking` join
    /// failure) must also fail open rather than poison the paste path.
    #[tokio::test(flavor = "current_thread")]
    async fn panicking_probe_fails_open() {
        let verdict = probe_with_budget(Duration::from_millis(300), || {
            panic!("probe blew up");
        })
        .await;
        assert_eq!(verdict, FocusProbe::Unknown);
    }

    /// Empty input must short-circuit before any pasteboard / event work, so
    /// the rest of the pipeline can rely on `paste("")` being a no-op rather
    /// than racing with permissions.
    #[tokio::test(flavor = "current_thread")]
    async fn paste_empty_returns_nothing_to_paste() {
        let injector = MacInjector::default();
        let err = injector.paste("").await.expect_err("empty must error");
        assert!(matches!(err, MuniError::NothingToPaste));
    }

    /// The paste delay still matches Swift v1 (50 ms), but the restore delay is
    /// deliberately *wider* than Swift v1's 120 ms. A stalled target app can
    /// read the pasteboard after an early restore and paste the user's previous
    /// clipboard instead of the dictation; this test pins the widened floor so a
    /// future refactor can't silently narrow it back. The restore runs after the
    /// paste is already on screen, so the wider window costs no user-perceived
    /// latency.
    #[test]
    fn default_restore_window_is_widened_past_swift_v1() {
        let injector = MacInjector::default();
        assert_eq!(injector.paste_delay_ms, DEFAULT_PASTE_DELAY_MS);
        assert_eq!(injector.restore_delay_ms, DEFAULT_RESTORE_DELAY_MS);
        assert!(
            injector.restore_delay_ms >= 300,
            "restore window must stay wide enough to survive a briefly stalled paste consumer"
        );
    }

    /// The injector's intrinsic paste-delay default and the settings-layer
    /// default (`paste.delay_ms`) must be the same number. The runtime injector
    /// reads the setting (falling back to the settings default), while tests and
    /// `MacInjector::default()` use the injector const — if the two drift, the
    /// paste delay a user gets silently diverges from what the code assumes.
    /// This test pins them together so a change to one forces a change to both.
    #[test]
    fn settings_default_matches_injector_default() {
        assert_eq!(
            crate::settings::DEFAULT_PASTE_DELAY_MS,
            DEFAULT_PASTE_DELAY_MS,
            "settings paste-delay default drifted from the injector default"
        );
        // And the value `default_for` actually hands out must match too — that's
        // the concrete fallback the boot wiring uses.
        assert_eq!(
            crate::settings::default_for(crate::settings::KEY_PASTE_DELAY_MS)
                .and_then(|v| v.as_u64()),
            Some(DEFAULT_PASTE_DELAY_MS)
        );
    }

    /// The clamp ceiling must sit above the default, so a normal configured
    /// value is never silently clamped — only pathological ones are.
    #[test]
    #[allow(clippy::assertions_on_constants)] // both are consts; the guard is intentional
    fn max_paste_delay_is_above_default() {
        assert!(MAX_PASTE_DELAY_MS > DEFAULT_PASTE_DELAY_MS);
    }

    /// Short pastes (below the per-char threshold) get exactly the base delay —
    /// the common case must not pay extra clipboard-restore latency beyond the
    /// base window.
    #[test]
    fn effective_restore_delay_short_text_is_base() {
        assert_eq!(effective_restore_delay_ms(350, 0), 350);
        assert_eq!(effective_restore_delay_ms(350, 9), 350);
        assert_eq!(effective_restore_delay_ms(350, 10), 351);
    }

    /// A larger payload widens the window: the receiving app needs longer to
    /// read a big string off the pasteboard, so the restore must wait
    /// proportionally longer before clobbering the clipboard.
    #[test]
    fn effective_restore_delay_scales_with_length() {
        let short = effective_restore_delay_ms(350, 100);
        let long = effective_restore_delay_ms(350, 2000);
        assert!(long > short, "longer text must widen the restore window");
        // 350 base + 1000/10 = 450.
        assert_eq!(effective_restore_delay_ms(350, 1000), 450);
    }

    /// The window is capped so a pathologically large paste can't stall the
    /// clipboard restore (and any auto-submit Enter after it) indefinitely.
    #[test]
    fn effective_restore_delay_is_capped() {
        assert_eq!(
            effective_restore_delay_ms(350, 1_000_000),
            MAX_RESTORE_DELAY_MS
        );
    }

    /// The scaled delay must never drop below the configured base: the whole
    /// point of the fix is that the restore window can only *widen* with size,
    /// never shrink below the floor that prevents the stale-clipboard race.
    #[test]
    fn effective_restore_delay_never_below_base() {
        for len in [0usize, 1, 50, 500, 5_000, 50_000] {
            assert!(
                effective_restore_delay_ms(350, len) >= 350,
                "restore window shrank below base for len={len}"
            );
        }
    }

    /// The pasteboard payload must always end in a single trailing space so
    /// consecutive dictations (or typed input following a paste) don't
    /// collide on the same character. Cleanup `.trim()`s its output, so the
    /// only space at the tail comes from us — removing this append would
    /// silently break the `dictate → paste → type` workflow.
    #[test]
    fn paste_payload_appends_trailing_space() {
        assert_eq!(
            paste_payload("this is dictation 1."),
            "this is dictation 1. "
        );
        assert_eq!(paste_payload("hello"), "hello ");
        assert_eq!(paste_payload(""), " ");
    }
}

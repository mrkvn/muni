---
version: alpha
name: Muni
description: >
  Personal cross-platform dictation app. Press-and-hold the hotkey, speak,
  release — the cleaned text is pasted into whichever app has focus. The
  app surface is intentionally small: a transient HUD pill near the screen
  bottom during a press, plus a settings window that is open only when the
  user has business there. Visual language follows shadcn/ui (New York
  style) on Tailwind v4 with the neutral base palette.

colors:
  background: "#FFFFFF"
  foreground: "#252525"
  card: "#FFFFFF"
  cardForeground: "#252525"
  popover: "#FFFFFF"
  popoverForeground: "#252525"
  primary: "#343434"
  primaryForeground: "#FBFBFB"
  secondary: "#F7F7F7"
  secondaryForeground: "#343434"
  muted: "#F7F7F7"
  mutedForeground: "#8E8E8E"
  accent: "#F7F7F7"
  accentForeground: "#343434"
  destructive: "#D33B2C"
  border: "#EBEBEB"
  input: "#EBEBEB"
  ring: "#B5B5B5"

  backgroundDark: "#252525"
  foregroundDark: "#FBFBFB"
  cardDark: "#343434"
  popoverDark: "#343434"
  primaryDark: "#EBEBEB"
  primaryForegroundDark: "#343434"
  secondaryDark: "#404040"
  mutedDark: "#404040"
  mutedForegroundDark: "#B5B5B5"
  accentDark: "#404040"
  destructiveDark: "#FF6F61"
  borderDark: "#FFFFFF1A"
  inputDark: "#FFFFFF26"
  ringDark: "#8E8E8E"

  hudPillListening: "#000000B3"
  hudPillCleaning: "#0000008C"
  hudPillRecovering: "#D97706CC"
  hudBar: "#FFFFFFF2"
  hudDot: "#FFFFFFD9"
  hudSpinnerTrack: "#FFFFFF2E"
  hudSpinnerHead: "#FFFFFFE6"

typography:
  display:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 1.5rem
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: -0.01em
  heading:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 1.25rem
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: -0.01em
  subheading:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 1.125rem
    fontWeight: 600
    lineHeight: 1.3
  sectionTitle:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 0.9375rem
    fontWeight: 600
    lineHeight: 1.25
  body:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 1rem
    fontWeight: 400
    lineHeight: 1.5
  bodySmall:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 0.875rem
    fontWeight: 400
    lineHeight: 1.45
  caption:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 0.75rem
    fontWeight: 400
    lineHeight: 1.4
  navItem:
    fontFamily: "system-ui, -apple-system, 'SF Pro Text', sans-serif"
    fontSize: 0.875rem
    fontWeight: 500
    lineHeight: 1.25

rounded:
  none: 0px
  xs: 0.25rem
  sm: 0.375rem
  md: 0.5rem
  lg: 0.625rem
  xl: 0.875rem
  full: 9999px

spacing:
  "0": 0px
  "1": 0.25rem
  "1.5": 0.375rem
  "2": 0.5rem
  "2.5": 0.625rem
  "3": 0.75rem
  "4": 1rem
  "5": 1.25rem
  "6": 1.5rem
  "7": 1.75rem
  "8": 2rem

components:
  button:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.primaryForeground}"
    typography: "{typography.bodySmall}"
    rounded: "{rounded.md}"
    height: 2.25rem
    padding: "0.5rem 1rem"
  buttonHover:
    backgroundColor: "#343434E6"
    textColor: "{colors.primaryForeground}"
  buttonSecondary:
    backgroundColor: "{colors.secondary}"
    textColor: "{colors.secondaryForeground}"
    rounded: "{rounded.md}"
    height: 2.25rem
  buttonOutline:
    backgroundColor: "{colors.background}"
    textColor: "{colors.foreground}"
    rounded: "{rounded.md}"
    height: 2.25rem
  buttonDestructive:
    backgroundColor: "{colors.destructive}"
    textColor: "#FFFFFF"
    rounded: "{rounded.md}"
    height: 2.25rem
  buttonGhost:
    backgroundColor: "transparent"
    textColor: "{colors.foreground}"
    rounded: "{rounded.md}"
    height: 2.25rem
  buttonLink:
    backgroundColor: "transparent"
    textColor: "{colors.primary}"
    typography: "{typography.bodySmall}"
  buttonXs:
    height: 1.5rem
    padding: "0 0.5rem"
    typography: "{typography.caption}"
  buttonSmall:
    height: 2rem
    padding: "0.375rem 0.75rem"
  buttonLarge:
    height: 2.5rem
    padding: "0.5rem 1.5rem"
  buttonIcon:
    height: 2.25rem
    width: 2.25rem
    padding: 0px
  buttonIconXs:
    height: 1.5rem
    width: 1.5rem
    padding: 0px
  input:
    backgroundColor: "transparent"
    textColor: "{colors.foreground}"
    rounded: "{rounded.md}"
    height: 2.25rem
    padding: "0.25rem 0.75rem"
    typography: "{typography.bodySmall}"
  inputFocus:
    backgroundColor: "transparent"
    textColor: "{colors.foreground}"
  card:
    backgroundColor: "{colors.card}"
    textColor: "{colors.cardForeground}"
    rounded: "{rounded.xl}"
    padding: "1.5rem"
  settingsSection:
    textColor: "{colors.foreground}"
    typography: "{typography.sectionTitle}"
  settingsSectionRow:
    textColor: "{colors.foreground}"
    typography: "{typography.bodySmall}"
    padding: "0.625rem 0"
  sidebar:
    backgroundColor: "#FFFFFF66"
    textColor: "{colors.foreground}"
    width: 14rem
    padding: "0.75rem"
  navItem:
    backgroundColor: "transparent"
    textColor: "{colors.mutedForeground}"
    typography: "{typography.navItem}"
    rounded: "{rounded.md}"
    padding: "0.375rem 0.5rem"
  navItemActive:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accentForeground}"
    rounded: "{rounded.md}"
  hudPillListening:
    backgroundColor: "{colors.hudPillListening}"
    rounded: "{rounded.full}"
    height: 34px
    padding: "0 0.75rem"
  hudPillCleaning:
    backgroundColor: "{colors.hudPillCleaning}"
    rounded: "{rounded.full}"
    height: 34px
    padding: "0 0.75rem"
  hudPillRecovering:
    backgroundColor: "{colors.hudPillRecovering}"
    rounded: "{rounded.full}"
    height: 34px
    padding: "0 0.75rem"
  hudBar:
    backgroundColor: "{colors.hudBar}"
    rounded: "{rounded.full}"
    width: 3px
  toast:
    backgroundColor: "{colors.popover}"
    textColor: "{colors.popoverForeground}"
    rounded: "{rounded.lg}"
    padding: "0.75rem 1rem"
---

## Overview

Muni is a press-to-talk dictation app. Most of the time the app is invisible
— the user is in another application, holds the hotkey, speaks, releases,
and cleaned text appears in the focused field. The visual surface of Muni
itself therefore splits cleanly in two:

- **The HUD overlay.** A small transparent window pinned near the screen
  bottom that mounts the *only* live UI during a press: a single pill that
  switches variant as the session progresses (listening → cleaning →
  recovering). Speed and quiet are the entire brief — the pill must appear
  and disappear without claiming the user's attention.
- **The Settings shell.** A standard desktop window opened on demand
  (tray, hotkey, or first-run wizard) where the user manages keys, custom
  vocabulary, hotkey choice, history, and cost views. The shell follows
  shadcn/ui (New York style) on Tailwind v4 with the neutral base palette
  so the look is unsurprising and matches macOS / Windows expectations.

The system as a whole leans **calm, neutral, and unbranded**. There is no
chrome color, no logotype mark, no decorative gradient — the work the app
does is the product. Color is reserved for state (focus rings, destructive
intent, the amber recovering state) so when something *is* tinted, it
means something.

## Colors

Light and dark variants both stay on a neutral grey ramp; the only chromatic
tokens are `destructive` (red, danger / remove actions) and the HUD
`recovering` amber (used for the fallback-ASR branch of the pipeline).

- **`background` / `foreground`.** The base canvas of every non-HUD window.
  Light defaults to pure white, dark to near-black neutral. Foreground is
  always within 1.5–2.0 luminance steps of background to avoid eye-searing
  contrast on a long-running settings window.
- **`card`.** The settings sidebar and Card containers. In light mode it is
  the same hex as background — separation comes from a 1px border, not a
  fill — which keeps the surface flat. In dark mode card lifts one step
  above background so cards read as cards.
- **`primary`.** The default Button fill. Deliberately a deep neutral, not a
  brand color: the primary action is "save", "apply", "continue", and the
  app doesn't want any of those decisions to look exciting.
- **`secondary` / `muted` / `accent`.** Three tokens that all resolve to the
  same near-white in light mode (and near-charcoal in dark). Each has a
  semantic role — secondary fills, muted body copy / disabled wells, accent
  for hovered / active nav rows — even though the values overlap. Future
  redesigns can split them without renaming consumers.
- **`destructive`.** Red, used only for buttons that delete (API key
  removal, history wipe) and for `aria-invalid` ring states on inputs.
  Never used decoratively.
- **`border` / `input` / `ring`.** Greyscale, with `ring` lighter than
  `border` so focus is visible without being shouty. Focus uses a 3px ring
  with 50% opacity rather than a 1px solid stroke, so it reads as a glow.
- **HUD palette.** The HUD pill background uses partially transparent
  black (`#000000B3` listening, `#0000008C` cleaning) backed by a `backdrop-blur-md`,
  and amber (`#D97706CC`) for the recovering variant. The spectrum bars and
  processing dots are near-opaque white so they stay legible on whatever
  the user has behind the HUD window.

## Typography

A single sans stack — the OS system font — with six size roles. No bespoke
typeface, no web-font load. The settings window's longest single screen is
a paragraph of preference copy, so the type system is sized for *reading
short things quickly*, not long-form content.

- **`display`** (24px / 600). Used once: the Landing-route page title
  ("Muni"). It establishes "this is the top of the window."
- **`heading`** (20px / 600). Used for the top-of-pane title in each
  Settings route ("API Keys", "Substitutions", etc.). Always paired with
  `tracking-tight` to feel deliberate on macOS.
- **`subheading`** (18px / 600). Dialog titles (`DialogTitle`). Reserved for
  the modal layer — it is *not* used for in-route section titles.
- **`sectionTitle`** (15px / 600). Section headings inside a grouped Settings
  route — "Shortcut", "Permissions", "History" on General; each service on
  API Keys; "About me" / "Your preferences" on Cleanup. Proper case (never
  uppercased), full `foreground` color, sitting one notch above `bodySmall`
  so a section is findable at a glance without shouting. This is the heading
  half of the chrome-free "settings section" pattern (see Layout).
- **`body`** (16px / 400). Default text body. Used sparingly — most settings
  copy is `bodySmall`.
- **`bodySmall`** (14px / 400). The default voice of the app: nav labels,
  field labels, helper text, button text.
- **`caption`** (12px / 400). Helper micro-copy: input hints, status pills,
  cost-view per-row breakdown.

Weights are limited to 400 and 600. There is no italic and no underlined
body text; links use color (`primary`) plus an underline-on-hover, not a
persistent underline.

## Layout

The app composes against a 4px base unit (Tailwind's default). Container
widths and chrome dimensions follow shadcn defaults except where the use
case argues otherwise.

- **Settings window.** Two-column flex: a fixed-width 224px sidebar (`w-56`)
  with `border-r` separating it from a scrollable main column. The main
  column constrains content to `max-w-3xl` (~768px) centered with `px-8 py-8`.
  This is wide enough for a two-column key/value form, narrow enough that
  the eye doesn't have to sweep on a 14" laptop.
- **Vertical rhythm.** Two route shapes share one underlying scale:
  - **Grouped routes** (General, API Keys, Cleanup) use the *settings
    section* pattern — a `sectionTitle` heading hugging its rows, no card
    chrome. The spacing is a deliberate **proximity scale** so each gap is
    larger than the one nested inside it: heading→its rows ≈ 16px
    (`mb-1.5` + row `py-2.5`), sibling rows ≈ 20px apart (`py-2.5` + a
    hairline divider), and whole sections ≈ 38px apart (`mb-7`). The page
    title sits 20px (`mb-5`) above the first section. That ordering —
    16 < 20 < 38 — is what makes a borderless layout read as grouped; when
    the gaps were uniform, headings didn't visually "own" their rows.
  - **List / form routes** (History, Cost & Usage, Substitutions,
    Vocabulary) have no sub-sections, so they stay on a flat `gap-4` (16px)
    block rhythm with `gap-2` field clusters. About is centered at `gap-6`.
  - The shared units are still the same 6 / 8 / 10 / 16 / 20 / 28 px steps
    drawn from the spacing scale — one system, two densities.
- **Sidebar nav rows.** `px-2 py-1.5` per row, `gap-2` between icon and
  label, `gap-0.5` between rows. The cumulative density is roughly 32px
  per row — tight enough that the current eight routes (General,
  Cleanup, Vocabulary, Substitutions, History, Cost & Usage, API Keys,
  About) fit on one screen without a scrollbar, with headroom for one
  or two more before the row needs revisiting.
- **HUD window.** Fills the screen at the OS level, but the visible pill
  sits at `items-end justify-center` with `mb-2` — meaning it hugs the
  bottom edge of the screen by 8px. The pill itself is `h-34px` with
  `px-3`, the smallest size at which the spectrum bars and processing dots
  are still legible at typical viewing distance.

## Elevation & Depth

Muni's elevation system is intentionally shallow. Three levels exist:

- **Surface (no shadow).** The default. Settings panes, sidebar, list
  rows, form fields. Separation comes from borders or background-shade
  differences, not lift.
- **Card shadow (`shadow-sm`).** Cards on the Landing / debug panels in
  dev. A 1px-ish ambient shadow that says "this is a unit" without
  implying interactivity.
- **HUD blur (`backdrop-blur-md`).** The only "floating" surface in the
  product. The HUD pill is not on a canvas — it sits over the live
  desktop — so the visual depth cue is the OS-level blur of whatever is
  behind it. Backed by 55–80% opacity black (or 80% amber for recovering)
  to keep the foreground bars legible against any wallpaper.

Buttons use `shadow-xs` only in the `outline` and `secondary` variants to
hint the affordance; the default `primary` variant is flat — it's already
a high-contrast fill, so an additional shadow reads as noise.

## Shapes

Three radii do all the work.

- **`rounded-md` (8px-ish).** The default. Buttons, inputs, nav rows.
- **`rounded-lg` / `rounded-xl` (10–14px).** Cards, toasts, popovers — any
  multi-line container.
- **`rounded-full`.** The HUD pill and the spectrum bars *inside* it. The
  pill is the only fully round shape in the product, which makes the HUD
  visually distinct from the (rectangular) Settings world without needing
  a different palette.

Sharp 0-radius corners are reserved for separators and the window itself
(driven by the OS).

## Components

The component library is shadcn/ui (New York variant), generated with
`baseColor: neutral` and `cssVariables: true`. Components in use today,
in rough order of how often the user sees them:

- **Button** (`components/ui/button.tsx`). Six variants
  (`default | destructive | outline | secondary | ghost | link`) and
  eight sizes including four icon-only sizes (`default | xs | sm | lg |
  icon | icon-xs | icon-sm | icon-lg`). The default 36px height matches
  Input height so buttons can sit on the same row as a field without
  visual mismatch. The `xs` and `icon-xs` tiers (24px / `text-xs`) exist
  for in-row affordances in dense list editors (Substitutions, Vocabulary)
  where a 36px button would dominate the row.
- **Input** (`components/ui/input.tsx`). 36px tall, transparent fill with
  a 1px border. Focus is a 2px outer ring at 25% opacity plus a
  border-color swap to the ring color — intentionally quieter than the
  Button/Switch/Tabs focus treatment (3px at 50%) because the input is
  already a tall, high-frequency target and the heavier ring read as
  noisy when a long form was tabbed through. `aria-invalid` flips the
  same treatment to destructive. Textarea uses the same input focus
  rule.
- **SettingsSection** (`components/SettingsSection.tsx`). The chrome-free
  primitive for a group of related settings: a `sectionTitle` heading above
  a `divide-y` stack of rows. No border, no fill, no inset-legend card —
  separation is the bold heading plus hairline dividers *between* rows
  (`divide-y` skips the first/last edge) plus inter-section margin. Rows are
  passed as direct children, each owning its own `py-2.5`. This replaced the
  old `rounded-lg border` inset-legend fieldset cards on General and API
  Keys. Rendered as `<section role="group">` + `<h2>` rather than
  `<fieldset>`/`<legend>` to dodge a WebKit intrinsic-sizing reflow that
  made panes jump on the first interaction after a tab switch. (Bordered
  Cards still exist for genuine content units — the Cost & Usage summary,
  History rows, onboarding steps — just not for settings groups.)
- **Label, Form, Switch, Slider, Progress, Separator, Tabs, Dialog,
  Textarea, Card, Sonner toaster.** Standard shadcn primitives, used as-is.
  All take their styling from the CSS variables in `globals.css`.
- **Sidebar nav row** (composed inline in `SettingsLayout.tsx`). Not a
  separate primitive — just a `NavLink` styled as a flex row with the
  active state mapped to `bg-accent text-accent-foreground`. Lucide icon
  at `h-4 w-4`, label at `bodySmall` weight 500.
- **HUD pill** (composed inline in `HudWindow.tsx`). A Framer Motion
  `motion.div` with three visual variants distinguished only by background
  fill. The tap-to-toggle locked-mode session (`listeningLocked`)
  deliberately renders the same `listening` pill rather than getting its
  own variant — an earlier prototype added a lock glyph and accent tint
  for peripheral-vision discoverability, but dogfood feedback pushed the
  chrome back to identical-to-PTT. The locked-mode UX surface is the
  gesture itself (re-tap, Esc, tray Cancel, 60s timeout), not the pill.
  - `listening` — black 70% + nine animated `SpectrumBars` driven by live
    mic amplitude, retargeting every ~80ms with a 60ms tween.
    Bars max at `MAX_BAR_PX` so loud audio doesn't blow out the pill height.
  - `cleaning` — black 55% + a `ProcessingIndicator` (nine pulsing dots
    + a small CSS-spun ring). Width is fixed so the pill doesn't reflow
    as state changes.
  - `recovering` — amber 80% + the same `ProcessingIndicator`. Same
    geometry as `cleaning`; only the fill differs so the eye reads
    "still working, but on a different code path."
  Entry/exit: `opacity` + `scale` + `y` over 120ms `easeOut`. The pill is
  also held mounted for a `PILL_MIN_MOUNT_MS` (320ms) floor so a press
  that fails synchronously still shows the entrance animation.
- **Onboarding stepper** (`windows/onboarding/OnboardingWizard.tsx`).
  Six linear steps each gating the Continue button. The chrome is just a
  `Progress` bar and two `Button`s; the per-step body composes existing
  primitives.

## Do's and Don'ts

**Do:**

- Treat the HUD as the product's signature surface. Anything that changes
  HUD timing, animation, or fill is a design decision — surface it.
- Use semantic tokens (`text-muted-foreground`, `bg-accent`, etc.), never
  raw hex. The OKLCH variables in `globals.css` are the single source of
  truth; the hex values in this file are the converted view for spec
  consumers.
- Pair Button height with Input height when they share a row (default 36px
  matches default 36px).
- Reach for `bodySmall` first. The app's voice is small text; promoting to
  `body` or `heading` should be a deliberate choice.
- Use the `destructive` token only for irreversible user actions
  (delete a key, wipe history). Validation errors use it too, but only
  in conjunction with `aria-invalid` so screen readers also pick it up.
- Group related settings with `SettingsSection`, not a bordered card. A
  `sectionTitle` heading + hairline row dividers carries the grouping; the
  proximity scale (heading hugs its rows, sections sit further apart) is
  what makes sections scannable. Keep section headings proper case.

**Don't:**

- Don't introduce a brand color. The primary fill is intentionally a deep
  neutral. If you find yourself reaching for blue/purple/etc. for an
  emphasis state, use weight, size, or background contrast instead.
- Don't decorate the HUD. No icons inside the pill, no text labels, no
  count indicators. The pill says only one thing: which phase of the
  pipeline is live. Extra signals belong in the Settings window.
- Don't ship a fourth HUD variant for a new pipeline branch without
  re-justifying the language. Today we have three because the user
  recovery path needs to look different from the happy path; future
  branches should fold into an existing variant before becoming a new
  one.
- Don't add shadows to flat surfaces to "make them pop." The system
  separates surfaces with border and color shade, not lift. Adding
  ambient shadow to a sidebar or pane will read as inconsistent.
- Don't hardcode pixel dimensions in components when a spacing or sizing
  token exists. The HUD's `34px` pill height and `3px` bar width are
  exceptions — both are visual constants tuned by eye, not derived from
  the spacing scale.
- Don't introduce a web font. The system stack is a feature: launches
  faster, renders correctly in every locale the user might dictate from,
  and matches the host OS chrome the user is already reading.

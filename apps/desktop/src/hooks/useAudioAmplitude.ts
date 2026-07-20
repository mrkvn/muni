/**
 * Plan 039 task 62 — the `useAudioAmplitude` hook body (a reactive-state
 * wrapper around `audio://amplitude`, originally for a developer overlay)
 * had zero consumers; removed as dead code. `AMPLITUDE_EVENT` stays: it's
 * the wire constant {@link "@/hooks/useSpectrumHeights"} subscribes to
 * directly (into a ref, bypassing React state — see that hook's doc for
 * why).
 */
export const AMPLITUDE_EVENT = "audio://amplitude";

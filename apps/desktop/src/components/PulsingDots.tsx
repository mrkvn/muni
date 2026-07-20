/**
 * Three dots that pulse sequentially using the same `hudDotPulse`
 * keyframe the HUD's processing indicator uses. Reused across the
 * Settings → API Keys and Onboarding "Save" buttons so the "Muni is
 * working" visual vocabulary stays consistent across surfaces.
 */
export function PulsingDots() {
  return (
    <span
      aria-hidden="true"
      className="inline-flex items-center gap-1"
      data-testid="api-keys-validating-dots"
    >
      {[0, 1, 2].map((i) => (
        <span
          key={i}
          className="block h-1 w-1 rounded-full bg-current"
          style={{
            animation: "hudDotPulse 1.4s ease-in-out infinite",
            animationDelay: `${(i * 0.16).toFixed(2)}s`,
          }}
        />
      ))}
    </span>
  );
}

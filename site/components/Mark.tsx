/** The app icon: 1K in a viewfinder's crop marks. The same artwork as
 * `assets/app-icon/icon.svg`, cropped to the icon's shape. */
export function Mark({ size = 28 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="100 100 824 824" aria-hidden focusable="false">
      <rect x="100" y="100" width="824" height="824" rx="185" fill="#171A20" />
      <path
        d="M230 330 V230 H330 M694 230 H794 V330 M794 694 V794 H694 M330 794 H230 V694"
        fill="none"
        stroke="#FFC24B"
        strokeWidth="36"
        strokeLinecap="square"
      />
      <path
        transform="translate(310.4 368) scale(0.72)"
        fill="#F4F3EE"
        d="M80 0 H170 V400 H70 V150 L5 185 L0 112 Z M240 0 H340 V175 L430 0 H550 L430 233 L560 400 H445 L340 265 V400 H240 Z"
      />
    </svg>
  );
}

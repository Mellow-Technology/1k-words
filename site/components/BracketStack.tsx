import styles from "./BracketStack.module.scss";

/** A five-frame exposure bracket, darkest first, as brightness over the
 * middle exposure. */
const BRACKET = [
  { ev: "−2", brightness: 0.42 },
  { ev: "−1", brightness: 0.68 },
  { ev: "0", brightness: 1 },
  { ev: "+1", brightness: 1.4 },
  { ev: "+2", brightness: 1.9 },
];

/** A little landscape: sky, a low sun and two ridges. */
function Scene() {
  return (
    <svg viewBox="0 0 120 80" preserveAspectRatio="xMidYMid slice" aria-hidden focusable="false">
      <rect width="120" height="80" fill="#6f8fb0" />
      <rect y="34" width="120" height="46" fill="#c9a26a" opacity="0.35" />
      <circle cx="84" cy="34" r="9" fill="#ffd27a" />
      <path d="M0 58 L22 44 L40 52 L62 36 L88 54 L120 42 V80 H0 Z" fill="#3e5468" />
      <path d="M0 68 L30 58 L56 66 L84 56 L120 64 V80 H0 Z" fill="#26323d" />
    </svg>
  );
}

function Frame({ brightness }: { brightness: number }) {
  return (
    <span className={styles.frame} style={{ filter: `brightness(${brightness})` }}>
      <Scene />
    </span>
  );
}

/**
 * The same bracket twice: as separate tiles, the way most libraries show
 * it, and as one tile, fanned open, its middle exposure on top.
 */
export function BracketStack() {
  return (
    <figure className={styles.compare}>
      <div className={styles.side}>
        <div className={styles.loose}>
          {BRACKET.map((shot) => (
            <div key={shot.ev} className={styles.looseShot}>
              <Frame brightness={shot.brightness} />
              <span className={styles.ev}>{shot.ev} EV</span>
            </div>
          ))}
        </div>
        <p className={styles.verdict}>
          <strong>Five tiles</strong> elsewhere, one per frame
        </p>
      </div>

      <div className={styles.side}>
        <div className={styles.fan}>
          {/* The cover goes last, so it's drawn on top. */}
          {[0, 4, 1, 3, 2].map((ix) => (
            <span key={ix} className={styles.fanFrame} data-position={ix - 2}>
              <Frame brightness={BRACKET[ix].brightness} />
            </span>
          ))}
          <span className={styles.badge}>
            <StackIcon />5
          </span>
        </div>
        <p className={styles.verdict}>
          <strong>One tile</strong> in 1k Words, its cover the 0 EV frame
        </p>
      </div>
      <figcaption className={styles.caption}>AEB ±2 EV · 5 frames</figcaption>
    </figure>
  );
}

/** Frames behind a frame, like the app's bracket badge. */
function StackIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden focusable="false" fill="none" stroke="currentColor" strokeWidth="1.3">
      <rect x="1" y="4" width="7" height="7" rx="1" />
      <path d="M3.5 2.5V1.5H10.5V8.5H9.5" />
    </svg>
  );
}

import styles from "./Shot.module.scss";

export interface ShotProps {
  /** The screenshot's file in `public/screenshots`, such as "library-grid.png".
   * Until it's there, leave this out to show a placeholder. */
  src?: string;
  /** What the screenshot shows: its alt text, and the placeholder's label. */
  alt: string;
  /** Width over height, such as 16 / 10. */
  ratio?: number;
  /** A caption under it, like the details printed on a contact sheet. */
  caption?: string;
  /** Load it straight away rather than as it nears the screen, for one at
   * the top of the page. */
  eager?: boolean;
  className?: string;
}

const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

/**
 * A screenshot of the app, framed like a print. Without `src` it's a
 * placeholder: an 18% grey card with crop marks, saying what goes there.
 */
export function Shot({ src, alt, ratio = 16 / 10, caption, eager = false, className }: ShotProps) {
  return (
    <figure className={[styles.shot, className].filter(Boolean).join(" ")}>
      <div className={styles.frame}>
        {src ? (
          <img
            src={`${basePath}/screenshots/${src}`}
            alt={alt}
            loading={eager ? "eager" : "lazy"}
            style={{ aspectRatio: ratio }}
          />
        ) : (
          <div
            className={styles.placeholder}
            role="img"
            aria-label={`Screenshot to come: ${alt}`}
            style={{ aspectRatio: ratio }}
          >
            <span className={styles.thirds} aria-hidden />
            <span className={styles.label}>
              <span className={styles.tag}>Screenshot</span>
              {alt}
            </span>
          </div>
        )}
        <span className={styles.corners} aria-hidden />
      </div>
      {caption && <figcaption className={styles.caption}>{caption}</figcaption>}
    </figure>
  );
}

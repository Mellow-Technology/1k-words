import { Badge } from "gui-mellow/Badge";
import { Button } from "gui-mellow/Button";
import { Credit } from "gui-mellow/Credit";
import { CtaBanner } from "gui-mellow/CtaBanner";
import { Feature, FeatureGrid } from "gui-mellow/FeatureGrid";
import { Hero } from "gui-mellow/Hero";
import { Section } from "gui-mellow/Section";
import { SiteFooter } from "gui-mellow/SiteFooter";
import { SiteHeader } from "gui-mellow/SiteHeader";
import { Steps } from "gui-mellow/Steps";
import { Table } from "gui-mellow/Table";
import { Terminal } from "gui-mellow/Terminal";
import { ThemeToggle } from "gui-mellow/ThemeToggle";
import { BracketStack } from "@/components/BracketStack";
import { Mark } from "@/components/Mark";
import { Shot } from "@/components/Shot";
import styles from "./page.module.scss";

const REPO = "https://github.com/Mellow-Technology/1k-words";

function Brand() {
  return (
    <a href="#top" className={styles.brand}>
      <Mark />
      1k Words
    </a>
  );
}

/** A feature's label, set like the markings on a camera. */
function Marking({ children }: { children: React.ReactNode }) {
  return <span className={styles.marking}>{children}</span>;
}

/** What 1k Words does differently from Apple Photos, as far as we know.
 * Only differences: what both do well isn't listed. */
const COMPARISON: Array<[string, string, string]> = [
  [
    "Exposure brackets",
    "One tile per bracket, from any camera, with the middle exposure on top",
    "One photo per frame",
  ],
  [
    "Bursts from a camera",
    "One tile per burst, found by timing and exposure, from any camera",
    "Grouped only when the iPhone marked them as a burst",
  ],
  [
    "Where originals live",
    "Plain folders by day, such as Originals/2026/10/15, on any drive",
    "Inside the Photos library package",
  ],
  ["Several libraries", "Side by side in the sidebar, each on its own drive", "One at a time, chosen as Photos opens"],
  ["Backup and sync", "Your own S3-compatible bucket, through sandboxed plugins", "iCloud Photos"],
  ["Source code", "Open source, under the LGPL-3.0", "Closed"],
];

export default function Home() {
  return (
    <>
      <SiteHeader
        brand={<Brand />}
        items={[
          { href: "#stacks", label: "Stacks" },
          { href: "#compare", label: "Compared" },
          { href: "#features", label: "Features" },
          { href: "#storage", label: "Storage" },
          { href: "#get-started", label: "Get started" },
        ]}
        actions={
          <>
            {/* On phones the theme follows the system's, and the banner and footer link to GitHub. */}
            <span className={styles.wideOnly}>
              <ThemeToggle />
            </span>
            <span className={styles.wideOnly}>
              <Button href={REPO} variant="secondary" size="sm" icon={<GitHubIcon />}>
                GitHub
              </Button>
            </span>
          </>
        }
      />

      <main id="top">
        <Hero
          size="full"
          align="center"
          pattern="theme"
          eyebrow="Free and open source · For macOS"
          title={
            <>
              Shoot the whole bracket. <em>See one photo.</em>
            </>
          }
          subtitle="1k Words is a photo library for people with real cameras. It stacks exposure brackets and bursts into single tiles, keeps your originals as plain files on your own drives, and backs them up to storage you choose."
          actions={
            <>
              <Button href="#get-started" size="lg">
                Get 1k Words
              </Button>
              <Button href="#compare" variant="secondary" size="lg">
                Compare with Apple Photos
              </Button>
            </>
          }
        >
          <Shot
            className={styles.heroShot}
            alt="The library grid, with brackets and bursts stacked into single tiles and their badges showing"
            caption="Library · masonry grid · dark appearance"
          />
        </Hero>

        <Section
          id="stacks"
          size="wide"
          eyebrow="Stacks"
          title={
            <>
              Twenty frames. <em>One tile.</em>
            </>
          }
          description="Bracket for HDR or hold the shutter down for a burst, and most libraries show you every frame, one after another. 1k Words reads each photo's exposure details and keeps the frames of one moment together."
        >
          <div className={styles.split}>
            <ul className={styles.kinds}>
              <li>
                <h3>Exposure brackets</h3>
                <p>
                  Shots seconds apart at different exposures become one stack, with the middle exposure as its cover.
                  Manual brackets are told apart by shutter speed, and back-to-back brackets are split where the
                  exposures start over.
                </p>
              </li>
              <li>
                <h3>Bursts</h3>
                <p>
                  Frames shot moments apart at the same exposure are kept together, from any camera, not only an
                  iPhone. The burst ends where the exposure changes or the shooting pauses.
                </p>
              </li>
              <li>
                <h3>RAW+JPEG and Live Photos</h3>
                <p>
                  A raw file and its JPEG are one photo, marked RAW+. A Live Photo&apos;s video plays when you hover
                  over it, HDR video included.
                </p>
              </li>
            </ul>
            <BracketStack />
          </div>
        </Section>

        <Section
          id="compare"
          size="wide"
          eyebrow="Compared"
          title="Where it differs from Apple Photos"
          description="Apple Photos is a lovely place for iPhone pictures. 1k Words is for when your photos come off a memory card."
        >
          <div className={styles.compare}>
            <Table
              columns={["", "1k Words", "Apple Photos"]}
              rows={COMPARISON.map(([what, ours, theirs]) => [
                <strong key="what">{what}</strong>,
                ours,
                <span key="theirs" className={styles.theirs}>
                  {theirs}
                </span>,
              ])}
              caption="Photos for macOS, as far as we know. If we've got something wrong, please open an issue."
            />
          </div>
        </Section>

        <Section
          id="features"
          size="wide"
          eyebrow="Features"
          title="Made for reviewing a day’s shoot"
          description="A native Mac app, built in Rust. The grid only draws the photos in view, so a big library scrolls as easily as a small one."
        >
          <div className={styles.shots}>
            <Shot
              alt="Quick preview over the grid: a bracket's middle exposure, its aperture, shutter speed and ISO, and every frame along the bottom"
              caption="Quick preview · Space"
            />
            <Shot
              alt="The viewer zoomed in, with the details sidebar: exposure, lens, albums and On This Device"
              caption="Viewer · details sidebar"
            />
          </div>
          <FeatureGrid min="300px" className={styles.features}>
            <Feature index={<Marking>Space</Marking>} title="A quick look">
              Press Space to see a photo over the grid, with its aperture, shutter speed and ISO, and every frame of a
              bracket or burst along the bottom.
            </Feature>
            <Feature index={<Marking>On device</Marking>} title="Search what’s in them">
              Type &ldquo;dog&rdquo; or &ldquo;bicycle&rdquo; to find them. Objects are found on your Mac by YOLO11, so
              photos never leave it, or by a vision model you choose, through a plugin.
            </Feature>
            <Feature index={<Marking>SD · USB</Marking>} title="Straight from the card">
              Cards, USB drives and cameras appear in the sidebar. Import skips photos you already have, files them by
              the day they were taken, and never leaves a half-copied file. Eject when you&apos;re done.
            </Feature>
            <Feature index={<Marking>Read-only</Marking>} title="Originals you can’t break">
              Your originals are locked on disk, so another app or a stray drag in Finder can&apos;t change or delete
              them by accident.
            </Feature>
            <Feature index={<Marking>RAW · HEIC</Marking>} title="Every format you shoot">
              Camera raw from most makers, HEIC from iPhones, JPEG, TIFF and more. Zoom in and the full-resolution
              photo loads, with only the part in view drawn, so even very large files stay smooth.
            </Feature>
            <Feature index={<Marking>Nested</Marking>} title="Albums in albums">
              Nest albums like folders, and have one show the photos of the albums inside it. A copy of your albums is
              kept in the library&apos;s folder, so they travel with it.
            </Feature>
          </FeatureGrid>
        </Section>

        <Section
          id="storage"
          size="wide"
          eyebrow="Storage"
          title={
            <>
              Your photos, <em>your drives,</em> your bucket
            </>
          }
          description="No account and no lock-in. A library is a folder you can open in Finder; backups go where you point them."
        >
          <ol className={styles.flow} aria-label="Where your photos are">
            <li>
              <strong>On your drives</strong>
              <span>
                Each library is a folder of originals, filed by day, on your Mac or an external drive. Keep as many as
                you like.
              </span>
            </li>
            <li>
              <strong>In your bucket</strong>
              <span>
                Back a library up to any S3-compatible storage. Thumbnails go first, then the originals. Plugins are
                sandboxed: they only reach the hosts you set.
              </span>
            </li>
            <li>
              <strong>On every Mac</strong>
              <span>
                Sync the catalog between your Macs through a catalog server, and each shows the library&apos;s
                thumbnails. Open a photo whose
                original is elsewhere and it&apos;s downloaded; pin an album to keep it on this Mac.
              </span>
            </li>
          </ol>
          <div className={styles.storageShot}>
            <Shot
              ratio={4 / 3}
              alt="Settings: a library backing up to an S3 bucket, with how syncing last went on each Mac"
              caption="Settings · Libraries · Sync"
            />
            <div className={styles.next}>
              <Badge tone="warning" dot>
                Coming next
              </Badge>
              <h3>Free up space on its own</h3>
              <p>
                Set how much of the drive originals may take, and 1k Words removes the ones you haven&apos;t wanted in
                longest, but only once enough backups are confirmed. Raw files and the extra frames of a bracket go
                first; pinned photos never do.
              </p>
            </div>
          </div>
        </Section>

        <Section
          id="get-started"
          size="wide"
          eyebrow="Get started"
          title="Build it in a few minutes"
          description="1k Words is built with Rust and runs best on macOS. There's no download yet, so for now you build it from source."
        >
          <div className={styles.split}>
            <Steps
              steps={[
                {
                  title: "Install Rust",
                  body: (
                    <>
                      From <a href="https://rustup.rs">rustup.rs</a>, if you don&apos;t have it yet. The right
                      toolchain is picked up from the repository.
                    </>
                  ),
                },
                {
                  title: "Install FFmpeg and libheif",
                  body: (
                    <>
                      With <a href="https://brew.sh">Homebrew</a>. They decode Live Photo videos and HEIC photos.
                    </>
                  ),
                },
                {
                  title: "Build and open the app",
                  body: "Clone the repository and run it. Make a library, then import from a card or a folder.",
                },
              ]}
            />
            <Terminal
              label="Terminal"
              lines={[
                "brew install ffmpeg libheif pkg-config",
                "git clone https://github.com/Mellow-Technology/1k-words",
                "cd 1k-words",
                "cargo run --release",
              ]}
            />
          </div>
        </Section>

        <section className={styles.cta}>
          <CtaBanner
            title="A thousand words, one frame at a time"
            actions={
              <>
                <Button href={REPO} icon={<GitHubIcon />}>
                  Get 1k Words
                </Button>
                <Button href={`${REPO}#readme`} variant="secondary">
                  Read the docs
                </Button>
              </>
            }
          >
            Free and open source under the LGPL-3.0. Your photos stay on your drives.
          </CtaBanner>
        </section>
      </main>

      <SiteFooter
        brand={<Brand />}
        columns={[
          {
            heading: "Project",
            links: [
              { href: REPO, label: "Source code" },
              { href: `${REPO}/issues`, label: "Issues" },
              { href: `${REPO}/blob/main/COPYING.LESSER`, label: "License" },
            ],
          },
          {
            heading: "On this page",
            links: [
              { href: "#stacks", label: "Stacks" },
              { href: "#compare", label: "Compared with Apple Photos" },
              { href: "#get-started", label: "Get started" },
            ],
          },
        ]}
      >
        <p>For people who shoot a lot of photos. © 2026 Mellow Technology.</p>
        <p>1k Words isn&apos;t affiliated with or endorsed by Apple. Apple Photos is a trademark of Apple Inc.</p>
        <p>
          <Credit />
        </p>
      </SiteFooter>
    </>
  );
}

function GitHubIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden focusable="false" fill="currentColor">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
    </svg>
  );
}

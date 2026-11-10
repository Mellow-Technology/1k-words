import type { Metadata } from "next";
import { Archivo, IBM_Plex_Mono, IBM_Plex_Sans } from "next/font/google";
import { ThemeScript } from "gui-mellow/ThemeToggle";
import "./theme.scss";
import "./site.scss";

// Archivo's width axis gives headings the wide, engraved look of the
// markings on a camera body.
const display = Archivo({ variable: "--font-display", subsets: ["latin"], axes: ["wdth"] });
const body = IBM_Plex_Sans({ variable: "--font-body", subsets: ["latin"], weight: ["400", "500", "600"] });
// For exposure details, as a camera would print them: f/8 · 1/250 s · ISO 100.
const mono = IBM_Plex_Mono({ variable: "--font-mono", subsets: ["latin"], weight: ["400", "500"] });

export const metadata: Metadata = {
  title: "1k Words · A picture is worth 1000 words",
  description:
    "For photographers who'd rather be shooting than sorting. Brackets stack themselves, search finds what's in your photos, and syncing just works, so you can focus on the stories your pictures tell.",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={`${display.variable} ${body.variable} ${mono.variable}`} suppressHydrationWarning>
      <head>
        <ThemeScript />
      </head>
      <body>{children}</body>
    </html>
  );
}

import type { NextConfig } from "next";

// 1k Words' website, published to GitHub Pages. Pages serves it from a
// subpath (/1k-words); the deploy workflow sets this from the Pages
// configuration. It's passed on to the page too, for the screenshots'
// URLs, which plain <img> tags don't prefix themselves.
const basePath = process.env.PAGES_BASE_PATH ?? "";

const config: NextConfig = {
  output: "export",
  basePath,
  trailingSlash: true,
  env: { NEXT_PUBLIC_BASE_PATH: basePath },
  transpilePackages: ["gui-mellow"],
  experimental: { optimizePackageImports: ["gui-mellow"] },
};

export default config;

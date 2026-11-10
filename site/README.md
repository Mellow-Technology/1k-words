# 1k Words website

The marketing site, a Next.js static export built with
[gui-mellow](https://github.com/Mellow-Technology/gui-mellow) and published to
GitHub Pages by `.github/workflows/pages.yml`. Its theme is `app/theme.scss`.

```sh
bun install
bun run dev     # http://localhost:3003
bun run build   # into out/
```

## Screenshots

The site shows a placeholder (an 18% grey card with crop marks) wherever a
screenshot isn't here yet. To add one, save it in `public/screenshots` and pass its file name
as `src` to its `<Shot>` in `app/page.tsx`.

| File | Where | Size | What it shows |
| --- | --- | --- | --- |
| `library-grid.jpg` | Hero | 2080 × 1388 | The library grid, square, with RAW+ and bracket badges showing. Dark appearance. |
| `quick-preview.jpg` | Features | 1946 × 1538 | Quick preview (Space) over the grid: a bracket's middle exposure, its exposure details, and every frame along the bottom. |
| `viewer.jpg` | Features | 1830 × 1206 | The viewer zoomed in, with the details sidebar: file, capture and exposure. |
| `sync-settings.png` | Storage | 2422 × 1738 | Settings, Sync: adding S3-compatible storage. |

Tips for good ones:

- Use a real library with a mix of brackets, bursts, RAW+JPEG pairs and
  Live Photos, so the badges show.
- Capture at 2x (Retina). JPEG keeps photos small; the hero is resized to
  2080 px wide, twice the width it's shown at.
- Pass each one's width over height as `ratio`, so it isn't cropped.
- Crop to the window, without the desktop. The site frames each one with
  crop marks and a shadow, so leave the window's own shadow out.

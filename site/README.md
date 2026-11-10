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

| File | Where | Ratio | What it shows |
| --- | --- | --- | --- |
| `library-grid.png` | Hero | 16:10 | The library grid, masonry, with brackets and bursts stacked and their badges showing. Dark appearance. |
| `quick-preview.png` | Features | 16:10 | Quick preview (Space) over the grid: a bracket's middle exposure, its exposure details, and every frame along the bottom. |
| `viewer.png` | Features | 16:10 | The viewer zoomed in, with the details sidebar: exposure, lens, albums and On This Device. |
| `sync-settings.png` | Storage | 4:3 | Settings, Libraries: a library syncing to an S3 bucket, with the Devices row showing each Mac. |

Tips for good ones:

- Use a real library with a mix of brackets, bursts, RAW+JPEG pairs and
  Live Photos, so the badges show.
- Capture at 2x (Retina) and at least 2560 px wide for the 16:10 shots.
- Crop to the window, without the desktop. The site frames each one with
  crop marks and a shadow, so leave the window's own shadow out.

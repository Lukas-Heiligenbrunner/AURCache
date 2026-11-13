# Vendored stylesheet tooling

The page loads these from the server it came from, not from a CDN, so the UI
renders on a network that cannot reach (or deliberately blocks) jsDelivr and
`cdn.tailwindcss.com`. `aurcache-api`'s build script embeds this directory as
`/vendor/`.

| File | Source |
| --- | --- |
| `daisyui-4.12.14.css` | `https://cdn.jsdelivr.net/npm/daisyui@4.12.14/dist/full.min.css` |
| `tailwindcss-3.4.17.js` | `https://cdn.tailwindcss.com/3.4.17` (Tailwind's "Play CDN" script) |

Both are byte-for-byte what those URLs served; do not edit them. To move to a
newer version, download the new file, delete the old one, and update the paths
in `../index.html` and the table above. The version is in the file name so a
browser holding an older copy cannot mistake it for the new one.

The Tailwind script compiles the CSS in the browser from the classes it sees
in the DOM, which is why class names built at runtime work at all. Replacing
it with a build-time Tailwind run would make the page lighter, but the class
names would then have to appear whole in the Rust sources.
